"""列出 PE 里的资源类型（看有没有 RT_MANIFEST / RT_VERSION）。

    python bench/pe_resources.py <exe> [...]
"""
import struct
import sys
from pathlib import Path

RT_NAMES = {1: "CURSOR", 2: "BITMAP", 3: "ICON", 4: "MENU", 5: "DIALOG",
            6: "STRING", 9: "ACCELERATOR", 10: "RCDATA", 12: "GROUP_CURSOR",
            14: "GROUP_ICON", 16: "VERSION", 24: "MANIFEST"}


def sections(d):
    e = struct.unpack_from("<I", d, 0x3C)[0]
    coff = e + 4
    _machine, nsec = struct.unpack_from("<HH", d, coff)
    size_opt = struct.unpack_from("<H", d, coff + 16)[0]
    opt = coff + 20
    sec = opt + size_opt
    out = []
    for i in range(nsec):
        b = sec + i * 40
        vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", d, b + 8)
        out.append((vaddr, vsize, rawptr, rawsize))
    return opt, out


def rva2off(secs, rva):
    for vaddr, vsize, rawptr, rawsize in secs:
        if vaddr <= rva < vaddr + max(vsize, rawsize):
            return rawptr + (rva - vaddr)
    return None


def main():
    for p in sys.argv[1:]:
        d = Path(p).read_bytes()
        opt, secs = sections(d)
        pe32p = struct.unpack_from("<H", d, opt)[0] == 0x20B
        rsrс_rva = struct.unpack_from("<II", d, opt + (112 if pe32p else 96) + 2 * 8)[0]
        off = rva2off(secs, rsrс_rva)
        print("===", p, "===")
        if not off or rsrс_rva == 0:
            print("   没有资源目录（无 manifest、无版本信息）")
            continue
        n_named, n_id = struct.unpack_from("<HH", d, off + 12)
        kinds = []
        for i in range(n_named + n_id):
            ent = off + 16 + i * 8
            name_id, _ofs = struct.unpack_from("<II", d, ent)
            kinds.append(RT_NAMES.get(name_id & 0x7FFFFFFF, str(name_id & 0x7FFFFFFF)))
        print("   资源类型：", ", ".join(kinds) if kinds else "（空）")


if __name__ == "__main__":
    main()
