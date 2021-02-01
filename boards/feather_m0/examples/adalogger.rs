#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

use feather_m0 as hal;
use panic_halt as _;

use alloc::string::String;
use core::alloc::Layout;
use core::fmt::Write;
use core::sync::atomic;

use alloc_cortex_m::CortexMHeap;
use cortex_m::interrupt::free as disable_interrupts;
use cortex_m::peripheral::NVIC;
use embedded_sdmmc::{Controller, SdMmcSpi, VolumeIdx};
use usb_device::bus::UsbBusAllocator;
use usb_device::prelude::*;
use usbd_serial::{SerialPort, USB_CLASS_CDC};

use hal::clock::{ClockGenId, ClockSource, GenericClockController};
use hal::delay::Delay;
use hal::entry;
use hal::pac::{interrupt, CorePeripherals, Peripherals};
use hal::prelude::*;
use hal::rtc;
use hal::time::U32Ext;
use hal::usb::UsbBus;

#[global_allocator]
static ALLOCATOR: CortexMHeap = CortexMHeap::empty();

#[entry]
fn main() -> ! {
    // setup basic peripherals
    let mut peripherals = Peripherals::take().unwrap();
    let mut core = CorePeripherals::take().unwrap();
    let mut clocks = GenericClockController::with_internal_32kosc(
        peripherals.GCLK,
        &mut peripherals.PM,
        &mut peripherals.SYSCTRL,
        &mut peripherals.NVMCTRL,
    );
    let mut delay = Delay::new(core.SYST, &mut clocks);

    // setup heap
    let start = cortex_m_rt::heap_start() as usize;
    let size = 10240; // in bytes
    unsafe { ALLOCATOR.init(start, size) }

    // configure the peripherals we'll need
    // get the internal 32k running at 1024 Hz for the RTC
    let timer_clock = clocks
        .configure_gclk_divider_and_source(ClockGenId::GCLK3, 32, ClockSource::OSC32K, true)
        .unwrap();
    let rtc_clock = clocks.rtc(&timer_clock).unwrap();
    let timer = rtc::Rtc::new(peripherals.RTC, rtc_clock.freq(), &mut peripherals.PM);
    let mut pins = hal::Pins::new(peripherals.PORT);
    let mut red_led = pins.d13.into_open_drain_output(&mut pins.port);

    let bus_allocator = unsafe {
        USB_ALLOCATOR = Some(hal::usb_allocator(
            peripherals.USB,
            &mut clocks,
            &mut peripherals.PM,
            pins.usb_dm,
            pins.usb_dp,
            &mut pins.port,
        ));
        USB_ALLOCATOR.as_ref().unwrap()
    };

    unsafe {
        USB_SERIAL = Some(SerialPort::new(&bus_allocator));
        USB_BUS = Some(
            UsbDeviceBuilder::new(&bus_allocator, UsbVidPid(0x16c0, 0x27dd))
                .manufacturer("Fake company")
                .product("Serial port")
                .serial_number("TEST")
                .device_class(USB_CLASS_CDC)
                .build(),
        );
    }

    unsafe {
        core.NVIC.set_priority(interrupt::USB, 1);
        NVIC::unmask(interrupt::USB);
    }

    // Now work on the SD peripherals
    let spi = hal::spi_master(
        &mut clocks,
        100_u32.khz(),
        peripherals.SERCOM4,
        &mut peripherals.PM,
        pins.sck,
        pins.mosi,
        pins.miso,
        &mut pins.port,
    );
    let sd_cd = pins.sd_cd.into_pull_up_input(&mut pins.port);
    let mut sd_cs = pins.sd_cs.into_open_drain_output(&mut pins.port);
    sd_cs.set_high().unwrap();

    while USB_DATA_RECEIVED.load(atomic::Ordering::Relaxed) == false {
        delay.delay_ms(250_u32);
        red_led.toggle();
    }

    if sd_cd.is_low().unwrap() {
        usbserial_write!("No card detected. Waiting...\r\n");
        while sd_cd.is_low().unwrap() {
            delay.delay_ms(250_u32);
        }
    }
    usbserial_write!("Card inserted!\r\n");
    delay.delay_ms(250_u32);

    let mut controller = Controller::new(SdMmcSpi::new(spi, sd_cs), timer);

    match controller.device().init() {
        Ok(_) => {
            usbserial_write!("OK!\r\nCard size...\r\n");
            match controller.device().card_size_bytes() {
                Ok(size) => usbserial_write!("{} bytes\r\n", size),
                Err(e) => usbserial_write!("Err: {:?}\r\n", e),
            }

            for i in 0..=3 {
                let volume = controller.get_volume(VolumeIdx(i));
                usbserial_write!("volume {:?}\r\n", volume);
                if let Ok(volume) = volume {
                    let root_dir = controller.open_root_dir(&volume).unwrap();
                    usbserial_write!("Listing root directory:\r\n");
                    controller
                        .iterate_dir(&volume, &root_dir, |x| {
                            usbserial_write!("\tFound: {:?}\r\n", x);
                        })
                        .unwrap();
                }
            }
        }
        Err(e) => usbserial_write!("Init err: {:?}!\r\n", e),
    }

    usbserial_write!("Done!\r\n");
    loop {
        delay.delay_ms(1_000_u32);
        red_led.toggle();
    }
}

/// Writes the given message out over USB serial.
#[macro_export]
macro_rules! usbserial_write {
    ($($tt:tt)*) => {{
        let mut s = String::new();
        write!(s, $($tt)*).unwrap();
        let message_bytes = s.as_bytes();
        let mut total_written = 0;
        while total_written < message_bytes.len() {
            let bytes_written = disable_interrupts(|_| unsafe {
                match USB_SERIAL.as_mut().unwrap().write(
                    &message_bytes[total_written..]
                ) {
                    Ok(count) => count,
                    Err(_) => 0,
                }
            });
            total_written += bytes_written;
        }
    }};
}

#[alloc_error_handler]
fn oom(_: Layout) -> ! {
    loop {}
}

static mut USB_ALLOCATOR: Option<UsbBusAllocator<UsbBus>> = None;
static mut USB_BUS: Option<UsbDevice<UsbBus>> = None;
static mut USB_SERIAL: Option<SerialPort<UsbBus>> = None;
static USB_DATA_RECEIVED: atomic::AtomicBool = atomic::AtomicBool::new(false);

#[interrupt]
fn USB() {
    unsafe {
        USB_BUS.as_mut().map(|usb_dev| {
            USB_SERIAL.as_mut().map(|serial| {
                usb_dev.poll(&mut [serial]);
                let mut buf = [0u8; 16];
                if let Ok(count) = serial.read(&mut buf) {
                    if count > 0 {
                        USB_DATA_RECEIVED.store(true, atomic::Ordering::Relaxed);
                    }
                }
            });
        });
    };
}
