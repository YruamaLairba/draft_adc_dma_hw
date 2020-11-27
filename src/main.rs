#![no_std]
#![no_main]

// pick a panicking behavior
//use panic_halt as _; // you can put a breakpoint on `rust_begin_unwind` to catch panics

// use panic_abort as _; // requires nightly
// use panic_itm as _; // logs messages over ITM; requires ITM support
// use panic_semihosting as _; // logs messages to the host stderr; requires a debugger

use crate::hal::{
    dma::{
        config::{DmaConfig, Priority},
        StreamsTuple, Transfer,
    },
    pac,
    prelude::*,
    stm32,
};

use core::cell::{Cell, RefCell};
use core::ptr;
use cortex_m::interrupt::Mutex;
use cortex_m::singleton;
use cortex_m_rt::entry;
use cortex_m_semihosting::*;
use rtt_target::{rprintln, rtt_init_print};
use stm32f4::stm32f411::{interrupt, NVIC};
use stm32f4xx_hal as hal;

static mut G_DMA_BUFFER: [u16; 16] = [0; 16];
static G_DATA: Mutex<RefCell<Option<&[u16]>>> = Mutex::new(RefCell::new(None));

#[entry]
fn main() -> ! {
    rtt_init_print!();
    let device = stm32::Peripherals::take().unwrap();
    let gpioa = device.GPIOA.split();
    let rcc = device.RCC.constrain();
    let _clocks = rcc.cfgr.sysclk(16.mhz()).pclk2(1.mhz()).freeze();

    let _pa0 = gpioa.pa0.into_analog();
    //power up the adc
    unsafe {
        let rcc = &(*pac::RCC::ptr());
        rcc.apb2enr.modify(|r, w| w.bits(r.bits() | (1 << 8)));
    }
    let adc = device.ADC1;
    //continuous mode and enable adc
    adc.cr2.modify(|_, w| {
        w.dma()
            .enabled()
            .cont()
            .continuous()
            .dds()
            .continuous()
            .adon()
            .enabled()
    });
    //sequence length = 1
    adc.sqr1.modify(|_, w| w.l().bits(0b0000_0000));
    //Use channel 0
    unsafe {
        adc.sqr3.modify(|_, w| w.sq1().bits(0b0000_0000));
    }
    adc.cr1.modify(|_, w| w.eocie().bit(false));
    adc.cr2.modify(|_, w| w.eocs().bit(false));
    //start conversion
    //adc prescaler /8
    device
        .ADC_COMMON
        .ccr
        .modify(|_, w| w.adcpre().bits(0b0000_0011));

    unsafe { NVIC::unmask(stm32f4::stm32f411::Interrupt::ADC) };
    let first_buffer = singleton!(: [u16; 128] = [0; 128]).unwrap();
    let second_buffer = singleton!(: [u16; 128] = [0; 128]).unwrap();
    let triple_buffer = Some(singleton!(: [u16; 128] = [0; 128]).unwrap());

    unsafe {
        let rcc = &(*pac::RCC::ptr());
        //reset DMA2
        rcc.ahb1rstr.modify(|r, w| w.bits(r.bits() | (1 << 22)));
        rcc.ahb1rstr.modify(|r, w| w.bits(r.bits() & !(1 << 22)));

        //enable DMA2 clock
        rcc.ahb1enr.modify(|r, w| w.bits(r.bits() | (1 << 22)));
    }

    let dma_2 = device.DMA2;
    //step 1, disable DMA
    //disable dma
    dma_2.st[0].cr.modify(|_, w| w.en().disabled());
    //wait the dma to be really disabled
    while dma_2.st[0].cr.read().en().bit_is_set() {}

    //reset stream 0 status
    dma_2.lifcr.write(|w| {
        w.ctcif0()
            .clear()
            .chtif0()
            .clear()
            .cteif0()
            .clear()
            .cdmeif0()
            .clear()
            .cfeif0()
            .clear()
    });

    //step 2, set the peripheral port register address
    dma_2.st[0]
        .par
        .modify(|_, w| unsafe { w.bits(&adc.dr as *const _ as u32) });

    //step 3, set the memory address
    dma_2.st[0]
        .m0ar
        .modify(|_, w| unsafe { w.bits(&G_DMA_BUFFER as *const _ as u32) });

    //step 4, set number of data to be transferred
    dma_2.st[0]
        .ndtr
        .modify(|_, w| unsafe { w.bits(G_DMA_BUFFER.len() as _) });

    //step 5, set DMA channel request
    dma_2.st[0].cr.modify(|_, w| w.chsel().bits(0));

    //step 6, select flow controller
    dma_2.st[0].cr.modify(|_, w| w.pfctrl().dma());

    //step 7, set stream priority
    dma_2.st[0].cr.modify(|_, w| w.pl().medium());

    //step 8, configure fifo usage
    dma_2.st[0].fcr.modify(|_, w| {
        w.dmdis().enabled() //direct mode, no fifo
    });

    //step 9
    dma_2.st[0].cr.modify(|_, w| {
        w
            //direction
            .dir()
            .peripheral_to_memory()
            //increment memory pointer ?
            .minc()
            .incremented()
            //increment peripheral pointer ?
            .pinc()
            .fixed()
            //memory burst
            .mburst()
            .single()
            //periph burst
            .pburst()
            .single()
            //periph data width
            .psize()
            .bits16()
            //memory data width
            .msize()
            .bits16()
            //circular mode ?
            .circ()
            .enabled()
            //double buffer ?
            .dbm()
            .disabled()
            //transfert complete interrupt
            .tcie()
            .enabled()
            //transfert complete interrupt
            .htie()
            .enabled()
            //error interrupts
            .teie()
            .enabled()
            .dmeie()
            .enabled()
    });

    //step 10 activate the stream
    dma_2.st[0].cr.modify(|_, w| w.en().enabled());

    //allow dma2 interrupt from processor side
    unsafe { NVIC::unmask(stm32f4::stm32f411::Interrupt::DMA2_STREAM0) };
    // Move the adc into our global storage
    //cortex_m::interrupt::free(|cs| *G_ADC.borrow(cs).borrow_mut() = Some(adc));
    adc.cr2.modify(|_, w| w.swstart().set_bit());

    rprintln!("Init Done");
    let last_dma_request = false;
    loop {
        let data:Option<()> = None;
        //let data = cortex_m::interrupt::free(|cs| G_DATA.borrow(cs).replace(None));
        rprintln!("{:?}", data);
        if let Some(data) = data {
            rprintln!("{:?}", data);
        }
        core::sync::atomic::spin_loop_hint();
    }
}

#[interrupt]
fn DMA2_STREAM0() {
    //hprintln!("DMA2_STREAM0").unwrap();
    return;

    let remain = unsafe {
        ptr::read_volatile(pac::DMA2::ptr()).st[0]
            .ndtr
            .read()
            .bits()
    };
    if remain == 16 {rprintln!("DMA2_STREAM0 {}", remain);}
    cortex_m::interrupt::free(|cs| match remain {
        8 => {
            G_DATA
                .borrow(cs)
                .replace(Some(unsafe { &G_DMA_BUFFER[0..8] }));
        }
        16 => {
            G_DATA
                .borrow(cs)
                .replace(Some(unsafe { &G_DMA_BUFFER[8..16] }));
        }
        _ => (),
    });
}

#[interrupt]
fn ADC() {
    unsafe {
        if ptr::read_volatile(pac::ADC1::ptr())
            .sr
            .read()
            .ovr()
            .bit_is_set()
        {
            rprintln!("ADC overrun");
        }
    }
}

use core::panic::PanicInfo;

#[inline(never)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    rprintln!("{}", info);
    loop {} // You might need a compiler fence in here.
}
