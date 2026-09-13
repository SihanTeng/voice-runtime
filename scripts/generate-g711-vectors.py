#!/usr/bin/env python3
"""Optional oracle regeneration with CPython 3.9–3.12; not required by tests."""
import audioop
from pathlib import Path
import struct

fixtures = Path(__file__).resolve().parent.parent / "tests" / "fixtures"
pcm = struct.pack("=65536h", *range(-32768, 32768))
for law in ("ulaw", "alaw"):
    (fixtures / f"g711-{law}-encode.bin").write_bytes(
        getattr(audioop, "lin2" + law)(pcm, 2)
    )
    native = getattr(audioop, law + "2lin")(bytes(range(256)), 2)
    (fixtures / f"g711-{law}-decode.pcm").write_bytes(
        struct.pack("<256h", *struct.unpack("=256h", native))
    )
