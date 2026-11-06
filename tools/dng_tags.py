"""Dump TIFF/DNG IFD structure: tag, type, count, short value preview. Debug aid, stdlib only."""
import struct, sys

def main(path):
    d = open(path, 'rb').read()
    e = '<' if d[:2] == b'II' else '>'
    sizes = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 6: 1, 7: 1, 8: 2, 9: 4, 10: 8, 11: 4, 12: 8, 13: 4}
    fmt = {1: 'B', 3: 'H', 4: 'I', 6: 'b', 8: 'h', 9: 'i', 11: 'f', 12: 'd', 13: 'I'}
    queue, seen = [(struct.unpack(e + 'I', d[4:8])[0], 'IFD0')], set()
    while queue:
        off, name = queue.pop(0)
        if off == 0 or off in seen:
            continue
        seen.add(off)
        n = struct.unpack(e + 'H', d[off:off + 2])[0]
        print(f'--- {name} @ {off}: {n} entries')
        for i in range(n):
            p = off + 2 + 12 * i
            tag, typ, cnt = struct.unpack(e + 'HHI', d[p:p + 8])
            size = sizes.get(typ, 1) * cnt
            vo = p + 8 if size <= 4 else struct.unpack(e + 'I', d[p + 8:p + 12])[0]
            raw = d[vo:vo + min(size, 64)]
            if typ == 2:
                val = raw.split(b'\0')[0].decode(errors='replace')
            elif typ in (5, 10):
                k = min(cnt, 8)
                v = struct.unpack(e + ('I' if typ == 5 else 'i') * 2 * k, d[vo:vo + 8 * k])
                val = [round(v[2 * j] / v[2 * j + 1], 5) if v[2 * j + 1] else None for j in range(k)]
            elif typ in fmt:
                k = min(cnt, 64 // sizes[typ])
                val = list(struct.unpack(e + fmt[typ] * k, d[vo:vo + sizes[typ] * k]))
            else:
                val = raw[:16].hex()
            print(f'  {tag:6d} type {typ:2d} count {cnt:8d}  {val}')
            if tag == 330:
                k = cnt
                for so in struct.unpack(e + 'I' * k, d[vo:vo + 4 * k]):
                    queue.append((so, f'SubIFD of {name}'))
        nxt = struct.unpack(e + 'I', d[off + 2 + 12 * n:off + 6 + 12 * n])[0]
        queue.append((nxt, f'next of {name}'))

if __name__ == '__main__':
    main(sys.argv[1])
