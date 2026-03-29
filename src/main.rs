use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::process;

use clap::Parser;
use serde::Serialize;

#[derive(Parser)]
#[command(name = "pgin", about = "Inspect PostgreSQL heap file pages")]
struct Args {
    /// Path to the PostgreSQL relation file
    file: PathBuf,

    /// Inspect a single page by index (exit 1 if out of range)
    #[arg(short = 'p', long = "page")]
    page: Option<usize>,

    /// Inspect a range of pages, e.g. 0-2 (zero-indexed, inclusive, clamps to available)
    #[arg(short = 'r', long = "range")]
    range: Option<String>,

    /// Show line pointer array and tuple data
    #[arg(short = 'i', long = "items")]
    items: bool,

    /// Show raw hex dump of each page alongside parsed output
    #[arg(short = 'x', long = "hex")]
    hex: bool,

    /// Override page size (default: 8192)
    #[arg(long = "page-size", default_value_t = 8192)]
    page_size: usize,

    /// Output format: text (default) or json
    #[arg(short = 'f', long = "format", default_value = "text")]
    format: String,

    /// Verify page checksums
    #[arg(long = "verify-checksums")]
    verify_checksums: bool,
}

const PAGE_HEADER_SIZE: usize = 24;

struct PageHeader {
    pd_lsn: u64,
    pd_checksum: u16,
    pd_flags: u16,
    pd_lower: u16,
    pd_upper: u16,
    pd_special: u16,
    pd_pagesize_version: u16,
    pd_prune_xid: u32,
}

impl PageHeader {
    fn from_bytes(buf: &[u8]) -> Self {
        let pd_lsn_raw = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        // High word is at byte 0, low word at byte 4 on disk (little-endian)
        // logical_lsn = (stored << 32) | (stored >> 32)
        let pd_lsn = (pd_lsn_raw << 32) | (pd_lsn_raw >> 32);

        PageHeader {
            pd_lsn,
            pd_checksum: u16::from_le_bytes(buf[8..10].try_into().unwrap()),
            pd_flags: u16::from_le_bytes(buf[10..12].try_into().unwrap()),
            pd_lower: u16::from_le_bytes(buf[12..14].try_into().unwrap()),
            pd_upper: u16::from_le_bytes(buf[14..16].try_into().unwrap()),
            pd_special: u16::from_le_bytes(buf[16..18].try_into().unwrap()),
            pd_pagesize_version: u16::from_le_bytes(buf[18..20].try_into().unwrap()),
            pd_prune_xid: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
        }
    }

    fn lsn_string(&self) -> String {
        let high = (self.pd_lsn >> 32) as u32;
        let low = self.pd_lsn as u32;
        format!("{}/{:X}", high, low)
    }

    fn page_version(&self) -> u16 {
        self.pd_pagesize_version & 0x00FF
    }

    fn page_size_from_header(&self) -> u16 {
        self.pd_pagesize_version & 0xFF00
    }

    fn free_space(&self) -> u16 {
        self.pd_upper.saturating_sub(self.pd_lower)
    }

    fn num_line_pointers(&self) -> usize {
        if self.pd_lower < PAGE_HEADER_SIZE as u16 {
            return 0;
        }
        ((self.pd_lower as usize) - PAGE_HEADER_SIZE) / 4
    }
}

#[derive(Debug, Clone)]
enum LpState {
    Unused,
    Normal,
    Redirect,
    Dead,
}

impl LpState {
    fn as_str(&self) -> &'static str {
        match self {
            LpState::Unused => "UNUSED",
            LpState::Normal => "NORMAL",
            LpState::Redirect => "REDIRECT",
            LpState::Dead => "DEAD",
        }
    }
}

#[derive(Debug, Clone)]
struct ItemId {
    index: usize,
    lp_off: u16,
    lp_flags: u8,
    lp_len: u16,
}

impl ItemId {
    fn from_u32(val: u32, index: usize) -> Self {
        let lp_off = (val & 0x7FFF) as u16;
        let lp_flags = ((val >> 15) & 0x3) as u8;
        let lp_len = ((val >> 17) & 0x7FFF) as u16;
        ItemId {
            index,
            lp_off,
            lp_flags,
            lp_len,
        }
    }

    fn state(&self) -> LpState {
        match self.lp_flags {
            0 => LpState::Unused,
            1 => LpState::Normal,
            2 => LpState::Redirect,
            3 => LpState::Dead,
            _ => unreachable!(),
        }
    }
}

#[derive(Serialize)]
struct JsonItemId {
    index: usize,
    offset: u16,
    length: u16,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    redirect_to: Option<u16>,
}

#[derive(Serialize)]
struct JsonPage {
    page: usize,
    offset: String,
    lsn: String,
    checksum: u16,
    flags: u16,
    lower: u16,
    upper: u16,
    special: u16,
    page_version: u16,
    prune_xid: u32,
    free_space: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_pointers: Option<Vec<JsonItemId>>,
}

fn parse_item_ids(buf: &[u8], header: &PageHeader) -> Vec<ItemId> {
    let n = header.num_line_pointers();
    let mut items = Vec::with_capacity(n);
    for i in 0..n {
        let off = PAGE_HEADER_SIZE + i * 4;
        let val = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        items.push(ItemId::from_u32(val, i));
    }
    items
}

fn print_hex_page(buf: &[u8], header: &PageHeader) {
    // Print each header field with byte ranges
    let lsn_str = header.lsn_string();
    println!(
        "  [0x00..0x07]  pd_lsn:      {}   LSN: {}",
        hex_bytes(&buf[0..8]),
        lsn_str
    );
    println!("  [0x08..0x09]  pd_checksum: {}", hex_bytes(&buf[8..10]));
    println!("  [0x0A..0x0B]  pd_flags:    {}", hex_bytes(&buf[10..12]));
    println!(
        "  [0x0C..0x0D]  pd_lower:    {}                     {}",
        hex_bytes(&buf[12..14]),
        header.pd_lower
    );
    println!(
        "  [0x0E..0x0F]  pd_upper:    {}                     {}",
        hex_bytes(&buf[14..16]),
        header.pd_upper
    );
    println!(
        "  [0x10..0x11]  pd_special:  {}                     {}",
        hex_bytes(&buf[16..18]),
        header.pd_special
    );
    println!(
        "  [0x12..0x13]  pd_pagesize_version: {}",
        hex_bytes(&buf[18..20])
    );
    println!("  [0x14..0x17]  pd_prune_xid: {}", hex_bytes(&buf[20..24]));
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_page_text(
    page_idx: usize,
    buf: &[u8],
    page_size: usize,
    show_items: bool,
    show_hex: bool,
) {
    let header = PageHeader::from_bytes(buf);
    let offset = page_idx * page_size;
    println!("Page {}  (offset 0x{:06X})", page_idx, offset);

    if show_hex {
        print_hex_page(buf, &header);
        return;
    }

    println!("  LSN:          {}", header.lsn_string());
    let checksum_note = if header.pd_checksum == 0 {
        " (disabled)"
    } else {
        ""
    };
    println!(
        "  Checksum:     0x{:04X}{}",
        header.pd_checksum, checksum_note
    );
    println!("  Flags:        0x{:04X}", header.pd_flags);
    println!(
        "  Lower:        {}  (0x{:04X})",
        header.pd_lower, header.pd_lower
    );
    println!(
        "  Upper:        {}  (0x{:04X})",
        header.pd_upper, header.pd_upper
    );
    println!(
        "  Special:      {}  (0x{:04X})",
        header.pd_special, header.pd_special
    );
    println!("  PageVersion:  {}", header.page_version());
    println!("  PruneXID:     {}", header.pd_prune_xid);
    println!("  FreeSpace:    {} bytes", header.free_space());
    println!("  LinePointers: {}", header.num_line_pointers());

    if show_items {
        println!();
        let items = parse_item_ids(buf, &header);
        for item in &items {
            match item.state() {
                LpState::Redirect => {
                    println!(
                        "  lp[{}]  offset={}  length={}  state={}  redirect_to={}",
                        item.index,
                        item.lp_off,
                        item.lp_len,
                        item.state().as_str(),
                        item.lp_off
                    );
                }
                _ => {
                    println!(
                        "  lp[{}]  offset={}  length={}  state={}",
                        item.index,
                        item.lp_off,
                        item.lp_len,
                        item.state().as_str()
                    );
                }
            }
        }
    }
}

fn build_json_page(page_idx: usize, buf: &[u8], page_size: usize, show_items: bool) -> JsonPage {
    let header = PageHeader::from_bytes(buf);
    let offset = page_idx * page_size;

    let line_pointers = if show_items {
        let items = parse_item_ids(buf, &header);
        Some(
            items
                .iter()
                .map(|item| {
                    let redirect_to = if matches!(item.state(), LpState::Redirect) {
                        Some(item.lp_off)
                    } else {
                        None
                    };
                    JsonItemId {
                        index: item.index,
                        offset: item.lp_off,
                        length: item.lp_len,
                        state: item.state().as_str().to_string(),
                        redirect_to,
                    }
                })
                .collect(),
        )
    } else {
        None
    };

    JsonPage {
        page: page_idx,
        offset: format!("0x{:06X}", offset),
        lsn: header.lsn_string(),
        checksum: header.pd_checksum,
        flags: header.pd_flags,
        lower: header.pd_lower,
        upper: header.pd_upper,
        special: header.pd_special,
        page_version: header.page_version(),
        prune_xid: header.pd_prune_xid,
        free_space: header.free_space(),
        line_pointers,
    }
}

fn parse_range(s: &str) -> Option<(usize, usize)> {
    let parts: Vec<&str> = s.splitn(2, '-').collect();
    if parts.len() != 2 {
        return None;
    }
    let start = parts[0].parse::<usize>().ok()?;
    let end = parts[1].parse::<usize>().ok()?;
    Some((start, end))
}

fn main() {
    let args = Args::parse();

    let mut file = match File::open(&args.file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error: cannot open {:?}: {}", args.file, e);
            process::exit(1);
        }
    };

    let mut data = Vec::new();
    if let Err(e) = file.read_to_end(&mut data) {
        eprintln!("Error: cannot read {:?}: {}", args.file, e);
        process::exit(1);
    }

    let page_size = args.page_size;
    let total_bytes = data.len();

    if total_bytes % page_size != 0 {
        eprintln!(
            "Warning: file size {} is not a multiple of page size {}; processing {} complete pages",
            total_bytes,
            page_size,
            total_bytes / page_size
        );
    }

    let num_pages = total_bytes / page_size;

    if num_pages == 0 {
        eprintln!("Error: file contains no complete pages");
        process::exit(1);
    }

    // Determine which pages to process
    let pages: Vec<usize> = if let Some(p) = args.page {
        if p >= num_pages {
            eprintln!(
                "Error: page {} is out of range (file has {} pages)",
                p, num_pages
            );
            process::exit(1);
        }
        vec![p]
    } else if let Some(ref range_str) = args.range {
        match parse_range(range_str) {
            Some((start, end)) => {
                let clamped_start = start.min(num_pages.saturating_sub(1));
                let clamped_end = end.min(num_pages.saturating_sub(1));
                (clamped_start..=clamped_end).collect()
            }
            None => {
                eprintln!("Error: invalid range format {:?}, expected N-M", range_str);
                process::exit(1);
            }
        }
    } else {
        (0..num_pages).collect()
    };

    // Warn about page size mismatches
    for &p in &pages {
        let buf = &data[p * page_size..(p + 1) * page_size];
        let header = PageHeader::from_bytes(buf);
        let header_page_size = header.page_size_from_header() as usize;
        if header_page_size != 0 && header_page_size != page_size {
            eprintln!(
                "Warning: page {} pd_pagesize_version indicates size {} but --page-size is {}",
                p, header_page_size, page_size
            );
        }
    }

    if args.format == "json" {
        let json_pages: Vec<JsonPage> = pages
            .iter()
            .map(|&p| {
                let buf = &data[p * page_size..(p + 1) * page_size];
                build_json_page(p, buf, page_size, args.items)
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_pages).unwrap());
    } else {
        if args.page.is_none() && args.range.is_none() {
            println!("File: {}  ({} pages)\n", args.file.display(), num_pages);
        }
        for &p in &pages {
            let buf = &data[p * page_size..(p + 1) * page_size];
            print_page_text(p, buf, page_size, args.items, args.hex);
            println!();
        }
    }
}
