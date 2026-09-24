//! What a PCI function says about itself, and where it lives.
//!
//! Pure: from the first sixty-four bytes of a function's configuration
//! space to something with names. The reading of those bytes is the
//! architecture's business (`arch::pci`); the meaning of them is not, and
//! a shift by four bits is exactly the kind of mistake that should be
//! caught by a test and not by a boot
//! (docs/adr/0022-fase4-pci-enumeration.md).

/// How many functions the kernel will keep. Enumerating is not allocating
/// (ADR 0022, point 5): running out is something to say, not something to
/// grow a `Vec` for in the middle of the boot.
pub const MAX_DEVICES: usize = 32;

/// A vendor of `0xFFFF` is how the bus says "nothing here": the read is
/// unclaimed and the bus returns all ones.
pub const NO_VENDOR: u16 = 0xFFFF;

/// Where a function lives on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Address {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Address {
    /// `device` must be below 32 and `function` below 8: the address has
    /// five bits for one and three for the other, and a number that does
    /// not fit would silently land on a different function.
    pub const fn new(bus: u8, device: u8, function: u8) -> Option<Self> {
        if device < 32 && function < 8 {
            Some(Self {
                bus,
                device,
                function,
            })
        } else {
            None
        }
    }

    /// The value to write to `0xCF8` to make `0xCFC` read the dword at
    /// `offset`. Bit 31 enables the mapping; the offset keeps only its
    /// dword-aligned part, because that port reads four bytes at a time.
    pub const fn config_address(self, offset: u8) -> u32 {
        1 << 31
            | (self.bus as u32) << 16
            | (self.device as u32) << 11
            | (self.function as u32) << 8
            | (offset as u32 & 0xFC)
    }
}

/// The header of a PCI function: who it is and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub vendor: u16,
    pub device: u16,
    /// What it does, coarsest first: class, then subclass, then the
    /// programming interface within that subclass.
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    /// Bit 7 says the device has more than one function; the low bits say
    /// what shape the rest of the header is.
    pub header_type: u8,
    /// Where its registers are, as the firmware left them. Untouched and
    /// unsized (ADR 0022, point 4).
    pub bars: [u32; 6],
}

impl Header {
    /// Decodes the first sixteen dwords of configuration space, or `None`
    /// when there is no function there.
    pub const fn from_config(words: &[u32; 16]) -> Option<Self> {
        let vendor = words[0] as u16;
        if vendor == NO_VENDOR {
            return None;
        }
        Some(Self {
            vendor,
            device: (words[0] >> 16) as u16,
            revision: words[2] as u8,
            prog_if: (words[2] >> 8) as u8,
            subclass: (words[2] >> 16) as u8,
            class: (words[2] >> 24) as u8,
            header_type: (words[3] >> 16) as u8,
            bars: [words[4], words[5], words[6], words[7], words[8], words[9]],
        })
    }

    /// Whether functions 1 to 7 are worth looking at. Asking a
    /// single-function device about its other functions can answer with a
    /// copy of function 0, which would list the same device eight times.
    pub const fn is_multifunction(self) -> bool {
        self.header_type & 0x80 != 0
    }

    /// The header layout below bit 7: 0 is an ordinary device, 1 a
    /// PCI-to-PCI bridge, 2 a CardBus bridge. Only 0 has six BARs.
    pub const fn layout(self) -> u8 {
        self.header_type & 0x7F
    }

    /// Enough of a name to make the boot log readable. Not a full table:
    /// the classes this kernel can meet, and a shrug for the rest.
    pub const fn class_name(self) -> &'static str {
        match (self.class, self.subclass) {
            (0x00, _) => "unclassified",
            (0x01, 0x00) => "SCSI storage",
            (0x01, 0x01) => "IDE storage",
            (0x01, 0x06) => "SATA storage",
            (0x01, 0x08) => "NVMe storage",
            (0x01, _) => "storage",
            (0x02, _) => "network",
            (0x03, _) => "display",
            (0x04, _) => "multimedia",
            (0x06, 0x00) => "host bridge",
            (0x06, 0x01) => "ISA bridge",
            (0x06, 0x04) => "PCI-to-PCI bridge",
            (0x06, _) => "bridge",
            (0x0C, 0x03) => "USB controller",
            (0x0C, _) => "serial bus",
            _ => "something else",
        }
    }
}

/// What a function turned out to be, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Function {
    pub at: Address,
    pub header: Header,
}

/// Every function the kernel found, in a table it never grows.
pub struct Devices {
    found: [Option<Function>; MAX_DEVICES],
    /// How many there were, including the ones that did not fit. The
    /// difference from `len()` is what was lost.
    seen: usize,
}

impl Default for Devices {
    fn default() -> Self {
        Self::new()
    }
}

impl Devices {
    pub const fn new() -> Self {
        Self {
            found: [None; MAX_DEVICES],
            seen: 0,
        }
    }

    /// Records a function, answering whether there was room. Counting
    /// happens either way: the number that did not fit is the useful part
    /// of running out.
    pub fn add(&mut self, function: Function) -> bool {
        self.seen += 1;
        match self.found.iter().position(Option::is_none) {
            Some(free) => {
                self.found[free] = Some(function);
                true
            }
            None => false,
        }
    }

    pub fn len(&self) -> usize {
        self.found.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many were found but not kept.
    pub fn lost(&self) -> usize {
        self.seen - self.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Function> {
        self.found.iter().flatten()
    }

    /// The first function with this vendor and device id, which is how a
    /// driver asks for the hardware it knows how to talk to.
    pub fn find(&self, vendor: u16, device: u16) -> Option<Function> {
        self.iter()
            .find(|f| f.header.vendor == vendor && f.header.device == device)
            .copied()
    }

    /// The first function of this class and subclass, for a driver that
    /// cares what a device does rather than who made it.
    pub fn find_class(&self, class: u8, subclass: u8) -> Option<Function> {
        self.iter()
            .find(|f| f.header.class == class && f.header.subclass == subclass)
            .copied()
    }
}

/// Where the configuration of a function is read from. The architecture
/// implements it — two port instructions on x86_64 — and everything above
/// this line is the same on any machine with a PCI bus, which is also
/// what makes the walk testable against a machine that does not exist.
pub trait ConfigSpace {
    /// The dword at `offset` of the function at `at`. Offsets are
    /// dword-aligned by construction; the low two bits are ignored.
    ///
    /// # Safety
    ///
    /// Reading configuration space has no effect on a device, but it does
    /// touch the machine: the implementor says what that costs and when it
    /// is allowed.
    unsafe fn read_dword(&self, at: Address, offset: u8) -> u32;
}

/// How many buses there can be. All of them are visited: a bus with
/// nothing on it costs thirty-two reads and says so.
const BUSES: u16 = 256;
const DEVICES_PER_BUS: u8 = 32;
const FUNCTIONS_PER_DEVICE: u8 = 8;
/// The header is the first sixteen dwords.
const HEADER_DWORDS: usize = 16;

/// Reads a function's header, or `None` when there is nothing there.
///
/// The vendor is read first, alone: if nothing answers, the other fifteen
/// reads would be fifteen reads of nothing.
///
/// # Safety
///
/// As `ConfigSpace::read_dword`.
pub unsafe fn read_header(space: &impl ConfigSpace, at: Address) -> Option<Header> {
    let mut words = [0u32; HEADER_DWORDS];
    // SAFETY: forwarded from this function's contract.
    words[0] = unsafe { space.read_dword(at, 0) };
    if words[0] as u16 == NO_VENDOR {
        return None;
    }
    for (index, word) in words.iter_mut().enumerate().skip(1) {
        // SAFETY: as above; `index * 4` stays under 64, inside the header.
        *word = unsafe { space.read_dword(at, (index * 4) as u8) };
    }
    Header::from_config(&words)
}

/// Every function on every bus, in the order they were found.
///
/// Functions 1 to 7 of a device are only asked about when function 0 says
/// the device is multifunction: a device with one function may answer for
/// all eight, and the same disk would be listed eight times.
///
/// # Safety
///
/// As `ConfigSpace::read_dword`.
pub unsafe fn scan(space: &impl ConfigSpace) -> Devices {
    let mut devices = Devices::new();
    for bus in 0..BUSES {
        let bus = bus as u8;
        for device in 0..DEVICES_PER_BUS {
            let Some(at) = Address::new(bus, device, 0) else {
                continue;
            };
            // SAFETY: forwarded from this function's contract.
            let Some(header) = (unsafe { read_header(space, at) }) else {
                continue;
            };
            let multifunction = header.is_multifunction();
            devices.add(Function { at, header });
            if !multifunction {
                continue;
            }
            for function in 1..FUNCTIONS_PER_DEVICE {
                let Some(at) = Address::new(bus, device, function) else {
                    continue;
                };
                // SAFETY: as above.
                if let Some(header) = unsafe { read_header(space, at) } {
                    devices.add(Function { at, header });
                }
            }
        }
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The word the kernel writes to `0xCF8`. Every field has its own
    /// shift, and a wrong one reads a different device — quietly, because
    /// the bus answers whatever is there.
    #[test]
    fn a_configuration_address_puts_every_field_where_it_belongs() {
        let at = Address::new(0, 0, 0).unwrap();
        assert_eq!(at.config_address(0), 0x8000_0000, "the enable bit");

        let at = Address::new(1, 2, 3).unwrap();
        assert_eq!(
            at.config_address(0x10),
            0x8000_0000 | 1 << 16 | 2 << 11 | 3 << 8 | 0x10
        );

        // The offset keeps only its dword-aligned part: that port reads
        // four bytes at a time and the low two bits would name a byte
        // inside them.
        let at = Address::new(0, 0, 0).unwrap();
        assert_eq!(at.config_address(0x13), at.config_address(0x10));
        assert_eq!(at.config_address(0x14) & 0xFF, 0x14);

        // The widest of everything, to show the fields do not collide.
        let at = Address::new(255, 31, 7).unwrap();
        assert_eq!(at.config_address(0xFC), 0x80FF_FFFC);
    }

    /// Five bits for the device and three for the function: a number that
    /// does not fit would land on a different function of a different
    /// device, which is worse than being refused.
    #[test]
    fn an_address_that_would_not_fit_is_refused() {
        assert!(Address::new(0, 31, 7).is_some());
        assert!(Address::new(0, 32, 0).is_none());
        assert!(Address::new(0, 0, 8).is_none());
        assert!(Address::new(255, 255, 255).is_none());
    }

    fn config_of(vendor: u16, device: u16, class: u32, header_type: u8) -> [u32; 16] {
        let mut words = [0u32; 16];
        words[0] = u32::from(vendor) | u32::from(device) << 16;
        words[2] = class;
        words[3] = u32::from(header_type) << 16;
        words
    }

    #[test]
    fn a_header_says_who_the_device_is_and_what_it_does() {
        // virtio-blk as QEMU presents it: Red Hat's vendor id, a modern
        // virtio device id, mass storage / SCSI.
        let mut config = config_of(0x1AF4, 0x1042, 0x0100_0002, 0x00);
        config[4] = 0xFEBD_1000;
        config[5] = 0x0000_C041;
        let header = Header::from_config(&config).expect("a device is there");
        assert_eq!(header.vendor, 0x1AF4);
        assert_eq!(header.device, 0x1042);
        assert_eq!(header.class, 0x01, "mass storage");
        assert_eq!(header.subclass, 0x00, "SCSI");
        assert_eq!(header.prog_if, 0x00);
        assert_eq!(header.revision, 0x02);
        assert_eq!(header.class_name(), "SCSI storage");
        assert_eq!(header.bars[0], 0xFEBD_1000);
        assert_eq!(header.bars[1], 0x0000_C041);
        assert_eq!(header.bars[5], 0);
        assert!(!header.is_multifunction());
        assert_eq!(header.layout(), 0);
    }

    #[test]
    fn nothing_there_is_not_a_device() {
        let mut config = [0u32; 16];
        config[0] = 0xFFFF_FFFF;
        assert_eq!(Header::from_config(&config), None);
        // Only the vendor half decides: a device id of all ones with a
        // real vendor is a real device.
        config[0] = 0xFFFF_1AF4;
        assert!(Header::from_config(&config).is_some());
    }

    #[test]
    fn multifunction_is_the_top_bit_and_the_layout_is_the_rest() {
        let bridge =
            Header::from_config(&config_of(0x8086, 0x7000, 0x0601_0000, 0x81)).expect("a bridge");
        assert!(bridge.is_multifunction());
        assert_eq!(bridge.layout(), 1, "a PCI-to-PCI bridge header");
        assert_eq!(bridge.class_name(), "ISA bridge");

        let plain =
            Header::from_config(&config_of(0x8086, 0x7000, 0x0601_0000, 0x01)).expect("a device");
        assert!(!plain.is_multifunction());
        assert_eq!(plain.layout(), 1);
    }

    fn function(bus: u8, vendor: u16, device: u16) -> Function {
        Function {
            at: Address::new(bus, 0, 0).unwrap(),
            header: Header::from_config(&config_of(vendor, device, 0x0100_0000, 0)).unwrap(),
        }
    }

    #[test]
    fn a_driver_asks_for_the_hardware_it_knows() {
        let mut devices = Devices::new();
        assert!(devices.is_empty());
        assert_eq!(devices.find(0x1AF4, 0x1042), None);

        assert!(devices.add(function(0, 0x8086, 0x1237)));
        assert!(devices.add(function(1, 0x1AF4, 0x1042)));
        assert_eq!(devices.len(), 2);
        assert_eq!(devices.lost(), 0);

        let found = devices.find(0x1AF4, 0x1042).expect("the disk");
        assert_eq!(found.at.bus, 1);
        assert_eq!(devices.find(0x1AF4, 0x1041), None, "a different device");
        assert_eq!(
            devices.find_class(0x01, 0x00).map(|f| f.header.vendor),
            Some(0x8086),
            "the first of that class, whoever made it"
        );
        assert_eq!(devices.find_class(0x02, 0x00), None);
    }

    /// Running out of room is a number to report, not a panic and not a
    /// silent truncation.
    #[test]
    fn what_does_not_fit_is_counted() {
        let mut devices = Devices::new();
        for n in 0..MAX_DEVICES {
            assert!(devices.add(function(n as u8, 0x1AF4, 0x1042)), "{n}");
        }
        assert_eq!(devices.len(), MAX_DEVICES);
        assert_eq!(devices.lost(), 0);

        assert!(!devices.add(function(200, 0x8086, 0x1237)), "no room left");
        assert!(!devices.add(function(201, 0x8086, 0x1237)));
        assert_eq!(devices.len(), MAX_DEVICES, "and it kept what it had");
        assert_eq!(devices.lost(), 2);
        // What was already there is still findable.
        assert!(devices.find(0x1AF4, 0x1042).is_some());
        assert_eq!(devices.find(0x8086, 0x1237), None, "the lost ones are lost");
    }

    /// A machine that does not exist: a table of what answers where, and a
    /// record of every read, so a test can say not only what the walk
    /// found but what it asked.
    #[derive(Default)]
    struct FakeMachine {
        functions: std::collections::BTreeMap<(u8, u8, u8), [u32; HEADER_DWORDS]>,
        reads: core::cell::RefCell<Vec<(u8, u8, u8, u8)>>,
    }

    impl FakeMachine {
        fn with(mut self, bus: u8, device: u8, function: u8, config: [u32; HEADER_DWORDS]) -> Self {
            self.functions.insert((bus, device, function), config);
            self
        }

        fn asked_about(&self, bus: u8, device: u8, function: u8) -> bool {
            self.reads
                .borrow()
                .iter()
                .any(|r| (r.0, r.1, r.2) == (bus, device, function))
        }
    }

    impl ConfigSpace for FakeMachine {
        unsafe fn read_dword(&self, at: Address, offset: u8) -> u32 {
            self.reads
                .borrow_mut()
                .push((at.bus, at.device, at.function, offset));
            match self.functions.get(&(at.bus, at.device, at.function)) {
                Some(config) => config[(offset / 4) as usize],
                // What a PCI bus answers for a function that is not there:
                // nobody claims the read, so every line is high.
                None => 0xFFFF_FFFF,
            }
        }
    }

    /// Everything on the machine, wherever it is, and nothing that is not.
    #[test]
    fn the_walk_finds_what_is_there_and_nothing_else() {
        let machine = FakeMachine::default()
            .with(0, 0, 0, config_of(0x8086, 0x1237, 0x0600_0000, 0x00))
            .with(0, 3, 0, config_of(0x1AF4, 0x1001, 0x0100_0000, 0x00))
            // Far from the others, to show the walk really visits them all.
            .with(255, 31, 0, config_of(0x1234, 0x1111, 0x0300_0000, 0x00));

        // SAFETY: the fake machine's reads touch nothing.
        let found = unsafe { scan(&machine) };
        assert_eq!(found.len(), 3);
        assert_eq!(found.find(0x8086, 0x1237).map(|f| f.at.device), Some(0));
        assert_eq!(found.find(0x1AF4, 0x1001).map(|f| f.at.device), Some(3));
        let far = found
            .find(0x1234, 0x1111)
            .expect("the last device of the last bus");
        assert_eq!((far.at.bus, far.at.device), (255, 31));
        assert_eq!(found.lost(), 0);
    }

    /// A device that does not say it is multifunction is not asked about
    /// its other seven: a single-function device may answer for all of
    /// them, and the same disk would be found eight times.
    #[test]
    fn only_multifunction_devices_are_asked_about_their_other_functions() {
        let quiet =
            FakeMachine::default().with(0, 1, 0, config_of(0x8086, 0x7010, 0x0101_0000, 0x00));
        // SAFETY: as above.
        let found = unsafe { scan(&quiet) };
        assert_eq!(found.len(), 1, "one device, once");
        assert!(quiet.asked_about(0, 1, 0));
        assert!(
            !quiet.asked_about(0, 1, 1),
            "it never said it had a function 1"
        );

        let talkative = FakeMachine::default()
            .with(0, 1, 0, config_of(0x8086, 0x7000, 0x0601_0000, 0x80))
            .with(0, 1, 1, config_of(0x8086, 0x7010, 0x0101_0000, 0x00))
            .with(0, 1, 3, config_of(0x8086, 0x7113, 0x0680_0000, 0x00));
        // SAFETY: as above.
        let found = unsafe { scan(&talkative) };
        assert_eq!(found.len(), 3, "the function that said so, and its others");
        assert!(talkative.asked_about(0, 1, 7), "all seven were asked");
        assert_eq!(found.find(0x8086, 0x7113).map(|f| f.at.function), Some(3));
    }

    /// A bus with nothing on it is asked once per device and costs nothing
    /// else: the header of a function that is not there is never read past
    /// its vendor.
    #[test]
    fn nothing_there_is_read_once_and_left_alone() {
        let empty = FakeMachine::default();
        // SAFETY: as above.
        let found = unsafe { scan(&empty) };
        assert!(found.is_empty());
        let reads = empty.reads.borrow();
        assert_eq!(
            reads.len(),
            256 * 32,
            "one read per device slot, and not one more"
        );
        assert!(
            reads.iter().all(|r| r.3 == 0),
            "only the vendor was ever asked for"
        );
    }

    /// More functions than the table holds: what fits is kept, the rest is
    /// counted, and the walk does not stop early.
    #[test]
    fn a_crowded_machine_fills_the_table_and_counts_the_rest() {
        let mut machine = FakeMachine::default();
        for device in 0..(MAX_DEVICES as u8 + 4) {
            machine = machine.with(
                0,
                device % 32,
                device / 32,
                config_of(0x1AF4, 0x1000 + u16::from(device), 0x0100_0000, 0x80),
            );
        }
        // SAFETY: as above.
        let found = unsafe { scan(&machine) };
        assert_eq!(found.len(), MAX_DEVICES);
        assert_eq!(found.lost(), 4);
        assert!(
            found.find(0x1AF4, 0x1000).is_some(),
            "the first still there"
        );
    }
}
