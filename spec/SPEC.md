# pgin — spec

A CLI tool for inspecting PostgreSQL heap file pages, written in Rust.

## Invocation

```
pgin [OPTIONS] <file>
```

### Arguments

| Argument | Description |
|---|---|
| `<file>` | Path to the PostgreSQL relation file (e.g. `base/16384/16411`) |

### Options

| Flag | Description |
|---|---|
| `-p, --page <N>` | Inspect a single page by index (default: all pages) |
| `-r, --range <N-M>` | Inspect a range of pages |
| `-i, --items` | Show line pointer array and tuple data |
| `-x, --hex` | Show raw hex dump of each page alongside parsed output |
| `--page-size <N>` | Override page size (default: 8192) |
| `-f, --format <fmt>` | Output format: `text` (default), `json` |

## Output

### Default (text, header only)

```
File: base/16384/16411  (3 pages)

Page 0  (offset 0x000000)
  LSN:          0/18E4A10
  Checksum:     0x0000 (disabled)
  Flags:        0x0000
  Lower:        64  (0x0040)
  Upper:        2816  (0x0B00)
  Special:      8192  (0x2000)
  PageVersion:  4
  PruneXID:     0
  FreeSpace:    2752 bytes
  LinePointers: 4

Page 1  (offset 0x002000)
  ...
```

### With `--items`

```
Page 0  (offset 0x000000)
  ...
  LinePointers: 4

  lp[0]  offset=2952  length=72  state=NORMAL
  lp[1]  offset=2880  length=72  state=NORMAL
  lp[2]  offset=2808  length=72  state=NORMAL  (dead)
  lp[3]  offset=0     length=0   state=UNUSED
```

### With `--hex`

Interleaves a hex dump of the raw 8KB block alongside the parsed fields,
with byte ranges annotated per field:

```
Page 0  (offset 0x000000)
  [0x00..0x07]  pd_lsn:      10 A1 E4 18 00 00 00 00   LSN: 0/18E4A10
  [0x08..0x09]  pd_checksum: 00 00
  [0x0A..0x0B]  pd_flags:    00 00
  [0x0C..0x0D]  pd_lower:    40 00                     64
  [0x0E..0x0F]  pd_upper:    00 0B                     2816
  [0x10..0x11]  pd_special:  00 20                     8192
  [0x12..0x13]  pd_pagesize_version: 04 20
  [0x14..0x17]  pd_prune_xid: 00 00 00 00
```

## Data structures (Rust)

```rust
// Mirrors PageHeaderData from postgres/src/include/storage/bufpage.h
#[repr(C)]
struct PageHeader {
    pd_lsn:               PageXLogRecPtr,  // u64 (hi: u32, lo: u32)
    pd_checksum:          u16,
    pd_flags:             u16,
    pd_lower:             u16,             // LocationIndex
    pd_upper:             u16,
    pd_special:           u16,
    pd_pagesize_version:  u16,
    pd_prune_xid:         u32,             // TransactionId
    // followed by pd_linp[]: ItemIdData[]
}

// ItemIdData: 32-bit packed field
// bits 0..1:   lp_flags  (UNUSED=0, NORMAL=1, REDIRECT=2, DEAD=3)
// bits 2..14:  lp_len    (byte length of tuple)
// bits 15..31: lp_off    (byte offset from page start)
struct ItemId(u32);
```

## Parsing strategy

1. Open file, determine size → number of pages (`size / page_size`)
2. For each selected page, read exactly `page_size` bytes into a `[u8; 8192]`
3. Parse `PageHeader` from the first 24 bytes using `bytemuck` or manual `u16::from_le_bytes`
4. Derive number of line pointers: `(pd_lower - size_of::<PageHeader>()) / 4`
5. Parse each `ItemId` (4 bytes, packed bitfield)
6. If `--items`: walk to `lp_off` within the page and print raw tuple bytes (no heap tuple decoding in v1)

## Error handling

- File not found or not readable → exit 1 with message
- File size not a multiple of `page_size` → warn, process complete pages only
- `pd_pagesize_version` page size bits disagree with `--page-size` → warn per page
- Checksum validation: if `pd_checksum != 0`, compute and compare (optional flag `--verify-checksums`)

## Phases / scope

| Phase | Scope |
|---|---|
| v1 | Parse and display page headers + line pointer array |
| v2 | Decode `HeapTupleHeader` (xmin, xmax, ctid, infomask, natts) |
| v3 | Decode tuple data given a user-supplied schema (`--schema "int4,text,float8"`) |
| v4 | TOAST pointer detection, FSM/VM file support |

## Dependencies (suggested)

- `clap` — argument parsing
- `bytemuck` — safe casting of `[u8]` to repr(C) structs
- `owo-colors` or `yansi` — colored output
- `serde` + `serde_json` — JSON output mode
