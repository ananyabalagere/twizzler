use pci_ids::FromId;
use twizzler_driver::{
    bus::pcie::{PcieFunctionHeader, PcieInfo},
    device::{BusType, Device},
};

static FOO: u64 = 0;
#[used]
static mut BAR: [u8; 0x2111] = [0; 0x2111];

fn get_pcie_offset(bus: u8, device: u8, function: u8) -> usize {
    ((bus as usize * 256) + (device as usize * 8) + function as usize) * 4096
}

fn print_info(bus: u8, slot: u8, function: u8, cfg: &PcieFunctionHeader) -> Option<()> {
    println!(
        "{} {} {}:: {:x} {:x} :: {:x} {:x} {:x}",
        bus,
        slot,
        function,
        cfg.vendor_id.get(),
        cfg.device_id.get(),
        cfg.class.get(),
        cfg.subclass.get(),
        cfg.progif.get(),
    );
    let device = pci_ids::Device::from_vid_pid(cfg.vendor_id.get(), cfg.device_id.get())?;
    let vendor = device.vendor();
    let class = pci_ids::Class::from_id(cfg.class.get())?;
    let subclass = pci_ids::Class::from_id(cfg.subclass.get())?;
    println!(
        "   => {} {}: {} {}",
        class.name(),
        subclass.name(),
        vendor.name(),
        device.name()
    );

    None
}

fn start_pcie(bus: Device) {
    println!("found pcie!");
    let info = unsafe { bus.get_info::<PcieInfo>(0).unwrap() };
    println!("{:?}", info.get_data());
    let mmio = bus.get_mmio(0).unwrap();

    for bus in 0..=255 {
        for device in 0..32 {
            let off = get_pcie_offset(bus, device, 0);
            let cfg = unsafe { mmio.get_mmio_offset::<PcieFunctionHeader>(off) };
            if cfg.vendor_id.get() != 0xffff
                && cfg.device_id.get() != 0xffff
                && cfg.vendor_id.get() != 0
            {
                let mf = if cfg.header_type.get() & 0x80 != 0 {
                    7
                } else {
                    0
                };
                for function in 0..=mf {
                    let off = get_pcie_offset(bus, device, function);
                    let cfg = unsafe { mmio.get_mmio_offset::<PcieFunctionHeader>(off) };
                    if cfg.vendor_id.get() != 0xffff {
                        print_info(bus, device, function, cfg);
                    }
                }
            }
        }
    }
    println!("done");
}

fn main() {
    unsafe {
        println!("Hello, world! {} {}", FOO, BAR.len());
    }

    for arg in std::env::args() {
        println!("arg: {}", arg);
    }

    let device_root = twizzler_driver::device::get_bustree_root();
    for device in device_root.children() {
        println!("{}", device);
        if device.is_bus() && device.bus_type() == BusType::Pcie {
            start_pcie(device);
        }
    }
}
