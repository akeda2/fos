pub fn vendor() -> [u8; 12] {
    let eax: u32 = 0;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    unsafe {
        core::arch::asm!(
            "push ebx",
            "cpuid",
            "mov edi, ebx",
            "pop ebx",
            inout("eax") eax => _,
            lateout("ecx") ecx,
            lateout("edx") edx,
            lateout("edi") ebx,
        );
    }
    let mut vendor = [0; 12];
    vendor[0..4].copy_from_slice(&ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&ecx.to_le_bytes());
    vendor
}

pub fn features() -> (u32, u32) {
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    unsafe {
        core::arch::asm!(
            "push ebx",
            "cpuid",
            "mov edi, ebx",
            "pop ebx",
            in("eax") 1u32,
            lateout("eax") _,
            lateout("ecx") ecx,
            lateout("edx") edx,
            lateout("edi") ebx,
        );
    }
    let _ = ebx;
    (edx, ecx)
}
