use core::{
    intrinsics::{copy_nonoverlapping, transmute},
    mem::size_of,
};

use crate::{
    aux::AuxEntry,
    object::{InternalObject, ObjID, Protections, MAX_SIZE, NULLPAGE_SIZE},
    slot::{RESERVED_DATA, RESERVED_STACK, RESERVED_TEXT},
    syscall::{
        BackingType, HandleType, LifetimeType, MapFlags, NewHandleFlags, ObjectCreate,
        ObjectCreateFlags, ObjectSource, ThreadSpawnArgs, ThreadSpawnFlags,
    },
};

#[derive(Debug)]
#[repr(C)]
struct ElfHeader {
    magic: [u8; 4],
    class: u8,
    data: u8,
    ident_version: u8,
    os_abi: u8,
    abi_version: u8,
    pad: [u8; 7],
    elf_type: u16,
    machine: u16,
    version: u32,
    entry: u64,
    phoff: u64,
    shoff: u64,
    flags: u32,
    ehsize: u16,
    phentsize: u16,
    phnum: u16,
    shentsize: u16,
    shnum: u16,
    shstrndx: u16,
}

impl ElfHeader {
    pub fn verify(&self) -> bool {
        self.magic == [0x7f, 'E' as u8, 'L' as u8, 'F' as u8]
            && self.version == 1
            && self.ident_version == 1
            && self.class == 2 /* 64-bit */
    }
}

#[derive(Debug, PartialEq, PartialOrd, Ord, Eq, Clone, Copy)]
enum PhdrType {
    Null = 0,
    Load = 1,
    Dynamic = 2,
    Interp = 3,
    Phdr = 6,
    Tls = 7,
}

impl TryFrom<u32> for PhdrType {
    type Error = ();
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Null,
            1 => Self::Load,
            2 => Self::Dynamic,
            3 => Self::Interp,
            6 => Self::Phdr,
            7 => Self::Tls,
            _ => Err(())?,
        })
    }
}

#[repr(C)]
#[derive(Debug)]
struct ElfPhdr {
    ptype: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    paddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

impl ElfPhdr {
    pub fn phdr_type(&self) -> PhdrType {
        self.ptype.try_into().unwrap_or(PhdrType::Null)
    }

    pub fn prot(&self) -> Protections {
        let mut p = Protections::empty();
        if self.flags & 1 != 0 {
            p.insert(Protections::EXEC);
        }
        if self.flags & 2 != 0 {
            p.insert(Protections::WRITE);
        }
        if self.flags & 4 != 0 {
            p.insert(Protections::READ);
        }
        p
    }
}

#[derive(Debug)]
pub struct ElfObject<'a> {
    hdr: &'a ElfHeader,
    base_raw: *const u8,
    obj: &'a InternalObject<ElfHeader>,
}

struct PhdrIter<'a> {
    elf: &'a ElfObject<'a>,
    pos: usize,
}

impl<'a> Iterator for PhdrIter<'a> {
    type Item = &'a ElfPhdr;

    fn next(&mut self) -> Option<Self::Item> {
        let n = self.pos;
        self.pos += 1;
        self.elf.get_phdr(n)
    }
}

impl<'a> ElfObject<'a> {
    fn verify(&self) -> bool {
        self.hdr.verify()
    }

    fn entry(&self) -> u64 {
        self.hdr.entry
    }

    fn ph_entry_size(&self) -> usize {
        self.hdr.phentsize as usize
    }

    fn get_phdr(&self, pos: usize) -> Option<&'a ElfPhdr> {
        if pos >= self.hdr.phnum as usize {
            return None;
        }
        let offset = pos * self.hdr.phentsize as usize + self.hdr.phoff as usize;
        Some(unsafe { transmute(self.base_raw.add(offset)) })
    }

    fn from_raw_memory(obj: &'a InternalObject<ElfHeader>, mem: *const u8) -> Option<Self> {
        let elf = Self {
            hdr: unsafe { transmute(mem) },
            base_raw: mem,
            obj,
        };
        if elf.verify() {
            Some(elf)
        } else {
            None
        }
    }

    fn from_obj(obj: &'a InternalObject<ElfHeader>) -> Option<Self> {
        let (start, _) = crate::slot::to_vaddr_range(obj.slot());
        Self::from_raw_memory(obj, start as *const u8)
    }

    fn phdrs(&self) -> PhdrIter {
        PhdrIter { elf: self, pos: 0 }
    }
}

const INITIAL_STACK_SIZE: usize = 1024 * 1024 * 4;

extern crate alloc;
pub fn spawn_new_executable(exe: ObjID, args: &[&[u8]], env: &[&[u8]]) -> Option<()> {
    let exe = InternalObject::<ElfHeader>::map(exe, Protections::READ)?;
    let elf = ElfObject::from_obj(&exe)?;

    let cs = ObjectCreate::new(
        BackingType::Normal,
        LifetimeType::Volatile,
        None,
        ObjectCreateFlags::empty(),
    );
    let vm_handle = crate::syscall::sys_object_create(cs, &[], &[]).unwrap();
    crate::syscall::sys_new_handle(vm_handle, HandleType::VmContext, NewHandleFlags::empty())
        .unwrap();

    let mut text_copy = alloc::vec::Vec::new();
    let mut data_copy = alloc::vec::Vec::new();
    let mut data_zero = alloc::vec::Vec::new();

    let page_size = NULLPAGE_SIZE as u64;

    for phdr in elf.phdrs().filter(|p| p.phdr_type() == PhdrType::Load) {
        let src_start = (phdr.offset & ((!page_size) + 1)) + NULLPAGE_SIZE as u64;
        let dest_start = phdr.vaddr & ((!page_size) + 1);
        let len = (phdr.filesz as u64 + (phdr.vaddr & (page_size - 1))) as usize;
        let aligned_len = len.checked_next_multiple_of(page_size as usize).unwrap();
        let copy = ObjectSource::new(exe.id(), src_start, dest_start, aligned_len);
        let prot = phdr.prot();

        if prot.contains(Protections::WRITE) {
            let brk = (phdr.vaddr & (page_size - 1)) + phdr.filesz;
            let pgbrk = (brk + (page_size - 1)) & ((!page_size) + 1);
            let pgend = (brk + phdr.memsz - phdr.filesz + (page_size - 1)) & ((!page_size) + 1);
            let dest_start = pgbrk & (MAX_SIZE as u64 - 1);
            let dest_zero_start = brk & (MAX_SIZE as u64 - 1);
            data_copy.push(copy);
            if pgend > pgbrk {
                data_copy.push(ObjectSource::new(
                    ObjID::new(0),
                    0,
                    dest_start,
                    (pgend - pgbrk) as usize,
                ))
            }
            data_zero.push((dest_zero_start, pgbrk - brk));
        } else {
            text_copy.push(copy);
        }
    }

    let text = crate::syscall::sys_object_create(cs, &text_copy, &[]).unwrap();
    let data = crate::syscall::sys_object_create(cs, &data_copy, &[]).unwrap();
    let stack = InternalObject::<()>::create_data_and_map()?;

    crate::syscall::sys_object_map(
        Some(vm_handle),
        text,
        RESERVED_TEXT,
        Protections::READ | Protections::EXEC,
        MapFlags::empty(),
    )
    .unwrap();
    crate::syscall::sys_object_map(
        Some(vm_handle),
        data,
        RESERVED_DATA,
        Protections::WRITE | Protections::READ,
        MapFlags::empty(),
    )
    .unwrap();
    crate::syscall::sys_object_map(
        Some(vm_handle),
        stack.id(),
        RESERVED_STACK,
        Protections::WRITE | Protections::READ,
        MapFlags::empty(),
    )
    .unwrap();

    let (stack_base, _) = crate::slot::to_vaddr_range(RESERVED_STACK);
    let spawnaux_start = stack_base + INITIAL_STACK_SIZE + page_size as usize;

    fn copy_strings<T>(stack: &InternalObject<T>, strs: &[&[u8]], offset: usize) -> (usize, usize) {
        let offset = offset.checked_next_multiple_of(64).unwrap();
        let (stack_base, _) = crate::slot::to_vaddr_range(RESERVED_STACK);
        let args_start = unsafe {
            let args_start: &mut () =
                stack.offset_from_base(INITIAL_STACK_SIZE + NULLPAGE_SIZE * 2 + offset);
            core::slice::from_raw_parts_mut(args_start as *mut () as *mut usize, strs.len() + 1)
        };
        let spawnargs_start = stack_base + INITIAL_STACK_SIZE + NULLPAGE_SIZE * 2 + offset;

        let args_data_start = unsafe {
            let args_data_start: &mut () = stack.offset_from_base(
                INITIAL_STACK_SIZE
                    + NULLPAGE_SIZE * 2
                    + offset
                    + size_of::<*const u8>() * (strs.len() + 1),
            );
            args_data_start as *mut () as *mut u8
        };
        let spawnargs_data_start = spawnargs_start + size_of::<*const u8>() * (strs.len() + 1);

        let mut data_offset = 0;
        for (i, arg) in strs.iter().enumerate() {
            let len = arg.len() + 1;
            unsafe {
                copy_nonoverlapping((*arg).as_ptr(), args_data_start.add(data_offset), len - 1);
                args_data_start.add(data_offset + len - 1).write(0);
            }
            args_start[i] = spawnargs_data_start + data_offset;
            data_offset += len;
        }
        args_start[strs.len()] = 0;
        let total = (spawnargs_data_start as usize + data_offset + 16) - spawnargs_start;
        (spawnargs_start, total)
    }

    /*
    let args_start = unsafe {
        let args_start: &mut () =
            stack.offset_from_base(INITIAL_STACK_SIZE + page_size as usize * 2);
        core::slice::from_raw_parts_mut(args_start as *mut () as *mut usize, args.len() + 1)
    };
    let spawnargs_start = stack_base + INITIAL_STACK_SIZE + page_size as usize * 2;

    let args_data_start = unsafe {
        let args_data_start: &mut () = stack.offset_from_base(
            INITIAL_STACK_SIZE + page_size as usize * 2 + size_of::<*const u8>() * (args.len() + 1),
        );
        args_data_start as *mut () as *mut u8
    };
    let spawnargs_data_start = spawnargs_start + size_of::<*const u8>() * (args.len() + 1);

    let mut offset = 0;
    for (i, arg) in args.iter().enumerate() {
        let len = arg.len() + 1;
        unsafe {
            copy_nonoverlapping(arg.as_ptr(), args_data_start.add(offset), len - 1);
            args_data_start.add(offset + len - 1).write(0);
        }
        args_start[i] = spawnargs_data_start + offset;
        offset += len;
    }
    args_start[args.len()] = 0;
    */
    let (spawnargs_start, args_len) = copy_strings(&stack, args, 0);
    let (spawnenv_start, _) = copy_strings(&stack, env, args_len);

    let aux_array = unsafe {
        stack.offset_from_base::<[AuxEntry; 32]>(INITIAL_STACK_SIZE + page_size as usize)
    };
    let mut idx = 0;

    if let Some(phinfo) = elf
        .phdrs()
        .filter(|p| p.phdr_type() == PhdrType::Phdr)
        .next()
    {
        aux_array[idx] =
            AuxEntry::ProgramHeaders(phinfo.vaddr, phinfo.memsz as usize / elf.ph_entry_size());
        idx += 1;
    }

    aux_array[idx] = AuxEntry::ExecId(exe.id());
    idx += 1;
    aux_array[idx] = AuxEntry::Arguments(args.len(), spawnargs_start as u64);
    idx += 1;
    aux_array[idx] = AuxEntry::Environment(spawnenv_start as u64);
    idx += 1;
    aux_array[idx] = AuxEntry::Null;

    let ts = ThreadSpawnArgs::new(
        elf.entry() as usize,
        stack_base,
        INITIAL_STACK_SIZE,
        0,
        spawnaux_start,
        ThreadSpawnFlags::empty(),
        Some(vm_handle),
    );
    unsafe {
        crate::syscall::sys_spawn(ts).unwrap();
    }
    //TODO: delete objects

    Some(())
}
