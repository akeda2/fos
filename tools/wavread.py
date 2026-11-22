"""Read a WAV file that QEMU's `-audiodev wav` backend wrote.

QEMU writes the header first, with the RIFF and data sizes at 0, and is meant to
patch them in at shutdown. QEMU 11 does not (the tests stop it with SIGTERM, and
a clean monitor `quit` leaves the sizes at 0 too), so Python's `wave` module sees
a RIFF chunk of 0 bytes and says "not a WAVE file". The samples are all there.

`open_wav(path)` is a drop-in for `wave.open(str(path))` for reading: it fixes
the two sizes in memory (a size of 0 or 0xFFFFFFFF means "to the end of the
file") and returns an ordinary `wave.Wave_read`. A well-formed file is returned
as it is, so the helper is safe for files written by anything else.
"""

import io
import struct
import wave


def open_wav(path):
    data = bytearray(open(path, "rb").read())
    if len(data) >= 12 and data[:4] == b"RIFF" and data[8:12] == b"WAVE":
        riff = struct.unpack_from("<I", data, 4)[0]
        if riff in (0, 0xFFFFFFFF):
            struct.pack_into("<I", data, 4, len(data) - 8)
        # Walk the chunks to the `data` chunk and fix its size the same way.
        pos = 12
        while pos + 8 <= len(data):
            tag = bytes(data[pos:pos + 4])
            size = struct.unpack_from("<I", data, pos + 4)[0]
            if tag == b"data":
                if size in (0, 0xFFFFFFFF) or pos + 8 + size > len(data):
                    size = len(data) - (pos + 8)
                    struct.pack_into("<I", data, pos + 4, size)
                break
            pos += 8 + size + (size & 1)
    return wave.open(io.BytesIO(bytes(data)), "rb")
