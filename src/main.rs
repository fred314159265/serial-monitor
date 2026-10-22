#![recursion_limit = "256"] // Needed for select!

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use futures::{future::FutureExt, select, StreamExt};
use mio_serial::SerialPortInfo;
use serialport::{SerialPortType, UsbPortInfo};
use std::convert::TryFrom;
use std::io;
use std::io::Write;
use std::result::Result as StdResult;
use structopt::StructOpt;
use tokio_serial::{DataBits, FlowControl, Parity, StopBits};
use tokio_util::bytes::Bytes;
use tokio_util::codec::BytesCodec;
use wildmatch::WildMatch;

mod error;
mod string_decoder;
use error::{ProgramError, Result};
use string_decoder::StringDecoder;

#[derive(StructOpt, Debug)]
#[structopt(name = "serial-monitor")]
struct Opt {
    /// Filter based on name of port
    #[structopt(short, long)]
    port: Option<String>,

    /// Baud rate to use
    #[structopt(short, long, default_value = "115200")]
    baud: u32,

    /// Turn on debugging
    #[structopt(short, long)]
    debug: bool,

    // Turn on local echo
    #[structopt(short, long)]
    echo: bool,

    /// List USB serial devices which are currently connected
    #[structopt(short, long)]
    list: bool,

    /// Enter character to send (cr, lf, crlf)
    #[structopt(long, default_value = "cr")]
    enter: Eol,

    /// Like list, but only prints the name of the port that was found.
    /// This is useful for using from scripts or makefiles.
    #[structopt(short, long)]
    find: bool,

    /// Turn on verbose messages
    #[structopt(short, long)]
    verbose: bool,

    /// Exit using Control-Y rather than Control-X
    #[structopt(short = "y")]
    ctrl_y_exit: bool,

    /// Filter based on Vendor ID (VID)
    #[structopt(long)]
    vid: Option<String>,

    /// Filter based on Product ID (PID)
    #[structopt(long)]
    pid: Option<String>,

    /// Filter based on manufacturer name
    #[structopt(short, long)]
    manufacturer: Option<String>,

    /// Filter based on serial number
    #[structopt(short, long)]
    serial: Option<String>,

    /// Filter based on product name
    #[structopt(long)]
    product: Option<String>,

    /// Return the index'th result
    #[structopt(long)]
    index: Option<usize>,

    /// Parity checking (none, odd, even)
    #[structopt(long, default_value = "none")]
    parity: ParityOpt,

    /// Stop bits (1, 2)
    #[structopt(long, default_value = "1")]
    stopbits: usize,

    /// Flow control (none, software, hardware)
    #[structopt(long, default_value = "none")]
    flow: FlowControlOpt,

    /// Data bits (5, 6, 7, 8)
    #[structopt(long, default_value = "8")]
    databits: usize,
}

struct DataBitsOpt(DataBits);

impl TryFrom<usize> for DataBitsOpt {
    type Error = io::Error;

    fn try_from(value: usize) -> StdResult<Self, io::Error> {
        match value {
            5 => Ok(Self(DataBits::Five)),
            6 => Ok(Self(DataBits::Six)),
            7 => Ok(Self(DataBits::Seven)),
            8 => Ok(Self(DataBits::Eight)),
            _ => Err(io::Error::other("databits out of range")),
        }
    }
}

/// Flow control modes
#[derive(Clone, Copy, Debug, StructOpt, strum::EnumString, strum::VariantNames)]
#[strum(serialize_all = "snake_case")]
enum FlowControlOpt {
    /// No flow control.
    None,
    /// Flow control using XON/XOFF bytes.
    Software,
    /// Flow control using RTS/CTS signals.
    Hardware,
}

impl From<FlowControlOpt> for FlowControl {
    fn from(opt: FlowControlOpt) -> Self {
        match opt {
            FlowControlOpt::None => FlowControl::None,
            FlowControlOpt::Software => FlowControl::Software,
            FlowControlOpt::Hardware => FlowControl::Hardware,
        }
    }
}

#[derive(Clone, Copy, Debug, StructOpt, strum::EnumString, strum::VariantNames)]
#[strum(serialize_all = "snake_case")]
enum ParityOpt {
    /// No parity bit.
    None,
    /// Parity bit sets odd number of 1 bits.
    Odd,
    /// Parity bit sets even number of 1 bits.
    Even,
}

impl From<ParityOpt> for Parity {
    fn from(opt: ParityOpt) -> Self {
        match opt {
            ParityOpt::None => Parity::None,
            ParityOpt::Odd => Parity::Odd,
            ParityOpt::Even => Parity::Even,
        }
    }
}

struct StopBitsOpt(StopBits);

impl TryFrom<usize> for StopBitsOpt {
    type Error = io::Error;

    fn try_from(value: usize) -> StdResult<Self, io::Error> {
        match value {
            1 => Ok(Self(StopBits::One)),
            2 => Ok(Self(StopBits::Two)),
            _ => Err(io::Error::other("stopbits out of range")),
        }
    }
}

/// End of line character options
#[derive(Debug, StructOpt, strum::EnumString, strum::VariantNames)]
#[strum(serialize_all = "snake_case")]
enum Eol {
    /// Carriage return
    Cr,
    /// Carriage return, line feed
    Crlf,
    /// Line feed
    Lf,
}

impl Eol {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Cr => &b"\r"[..],
            Self::Crlf => &b"\r\n"[..],
            Self::Lf => &b"\n"[..],
        }
    }
}

// Returns the lowercase version of the character which will cause
// serial-monitor to exit.
fn exit_char(opt: &Opt) -> char {
    if opt.ctrl_y_exit {
        'y'
    } else {
        'x'
    }
}

fn is_exit_event(event: &Event, opt: &Opt) -> bool {
    // Letter case and lock-key state are not part of the chord. A release is
    // not an exit: crossterm reports both edges, and only the press should stop.
    match event {
        Event::Key(key)
            if key.kind == KeyEventKind::Press && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            match key.code {
                KeyCode::Char(ch) => ch.eq_ignore_ascii_case(&exit_char(opt)),
                _ => false,
            }
        }
        _ => false,
    }
}

// Returns a human readable string of the exit character.
fn exit_label(opt: &Opt) -> String {
    format!("Control-{}", exit_char(opt).to_ascii_uppercase())
}

// Converts a byte string into a string comprised of each byte
// in hexadecimal, followed by a more human readable ASCII variant.
fn hex_str(bytes: &[u8]) -> String {
    let mut hex = String::from("");
    let mut ascii = String::from("");

    for byte in bytes.iter() {
        hex.push_str(&format!("{:02x} ", *byte));

        if *byte < 0x20 {
            if *byte == 0x1b {
                if !ascii.is_empty() {
                    ascii.push(' ');
                }
                ascii.push_str("ESC");
            } else {
                if !ascii.is_empty() {
                    ascii.push(' ');
                }
                ascii.push_str("Ctrl-");
                let ctrl = b"@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_";
                ascii.push(ctrl[*byte as usize] as char);
            }
        } else if *byte > b'~' {
            ascii.push('.');
        } else {
            ascii.push(*byte as char);
        }
    }

    hex.push(':');
    hex.push(' ');
    hex.push_str(&ascii);
    hex
}

fn has_wildcard(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

// A port name is exact unless the user wrote `*` or `?`, and the compare is
// case-sensitive. `COM1` must not select `COM10`.
fn port_name_matches(name: &str, pattern: Option<&str>, opt: &Opt) -> bool {
    let result = match pattern {
        None => true,
        Some(pattern) if has_wildcard(pattern) => WildMatch::new(pattern).matches(name),
        Some(pattern) => name == pattern,
    };
    if opt.debug {
        println!(
            "port(name:{:?}, pattern:{:?}) -> {:?}",
            name, pattern, result
        );
    }
    result
}

// Manufacturer, product, and serial are case-insensitive. Without a wildcard
// the pattern may match the middle of the field.
fn text_matches(value: Option<&str>, pattern: Option<&str>, opt: &Opt) -> bool {
    let result = match (value, pattern) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(value), Some(pattern)) if has_wildcard(pattern) => {
            WildMatch::new(&pattern.to_lowercase()).matches(&value.to_lowercase())
        }
        (Some(value), Some(pattern)) => value.to_lowercase().contains(&pattern.to_lowercase()),
    };
    if opt.debug {
        println!(
            "text(value:{:?}, pattern:{:?}) -> {:?}",
            value, pattern, result
        );
    }
    result
}

// VID and PID are 16-bit hex. `f055`, `F055`, and `0xf055` are the same value.
// `055` is a different value. A pattern with `*` or `?` is a case-insensitive
// glob on the 4-digit lowercase hex.
fn id_matches(id: u16, pattern: Option<&str>, opt: &Opt) -> bool {
    let rendered = format!("{:04x}", id);
    let result = match pattern {
        None => true,
        Some(pattern) => {
            let hex = pattern
                .strip_prefix("0x")
                .or_else(|| pattern.strip_prefix("0X"))
                .unwrap_or(pattern);
            if has_wildcard(hex) {
                WildMatch::new(&hex.to_lowercase()).matches(&rendered)
            } else {
                u16::from_str_radix(hex, 16).ok() == Some(id)
            }
        }
    };
    if opt.debug {
        println!("id(id:{}, pattern:{:?}) -> {:?}", rendered, pattern, result);
    }
    result
}

#[cfg(any(target_os = "macos", test))]
fn map_port_name(port_name: &str) -> String {
    // available_ports returns /dev/tty.* rather than /dev/cu.*
    // /dev/tty.* are designed for incoming serial connections and will block
    // until DCD is set.
    // /dev/cu.* are designed for outgoing serial connections and don't block,
    // so we change /dev/tty.* to /dev/cu.* since this program is primarily
    // used for outgoing connections.
    if port_name.starts_with("/dev/tty.") {
        port_name.replace("/dev/tty.", "/dev/cu.")
    } else {
        String::from(port_name)
    }
}

// Returns a list of the available ports (for macos)
#[cfg(target_os = "macos")]
fn available_ports() -> Result<Vec<SerialPortInfo>> {
    Ok(mio_serial::available_ports()?
        .into_iter()
        .map(|mut port| {
            port.port_name = map_port_name(&port.port_name);
            port
        })
        .collect())
}

// Returns a list of the available ports (for everything but macos)
#[cfg(not(target_os = "macos"))]
fn available_ports() -> Result<Vec<SerialPortInfo>> {
    Ok(mio_serial::available_ports()?)
}

fn requested_port_name(opt: &Opt) -> Option<String> {
    let name = {
        #[cfg(target_family = "unix")]
        {
            opt.port.as_deref().map(get_port_path)
        }
        #[cfg(not(target_family = "unix"))]
        {
            opt.port.clone()
        }
    };
    // macOS enumeration rewrites /dev/tty.* to /dev/cu.*. Apply the same
    // rewrite to the name the user typed so the two can be equal.
    #[cfg(target_os = "macos")]
    {
        name.map(|name| map_port_name(&name))
    }
    #[cfg(not(target_os = "macos"))]
    {
        name
    }
}

fn listed_port_name(port_name: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        map_port_name(port_name)
    }
    #[cfg(not(target_os = "macos"))]
    {
        port_name.to_owned()
    }
}

// Checks to see if a serial port matches the filtering criteria specified on the command line.
fn usb_port_matches(port: &SerialPortInfo, opt: &Opt) -> bool {
    let user_port_name = requested_port_name(opt);
    let listed_name = listed_port_name(&port.port_name);
    let name_ok = port_name_matches(&listed_name, user_port_name.as_deref(), opt);

    match &port.port_type {
        SerialPortType::UsbPort(info) => {
            name_ok
                && id_matches(info.vid, opt.vid.as_deref(), opt)
                && id_matches(info.pid, opt.pid.as_deref(), opt)
                && text_matches(
                    info.manufacturer.as_deref(),
                    opt.manufacturer.as_deref(),
                    opt,
                )
                && text_matches(info.serial_number.as_deref(), opt.serial.as_deref(), opt)
                && text_matches(info.product.as_deref(), opt.product.as_deref(), opt)
        }
        // PCI, Bluetooth, and unknown ports have no USB ids. They stay hidden
        // until the user names one with --port, and a USB-only filter excludes them.
        _ => {
            opt.port.is_some()
                && name_ok
                && opt.vid.is_none()
                && opt.pid.is_none()
                && opt.manufacturer.is_none()
                && opt.serial.is_none()
                && opt.product.is_none()
        }
    }
}

/// Get the destination of a symlink if the port name is indeed a path to a symlink.
///
/// If not a symlink, then will return the `port_name` untouched.
#[cfg(target_family = "unix")]
fn get_port_path(port_name: &str) -> String {
    let port_path = std::path::PathBuf::from(port_name);
    if let (true, Ok(link_target)) = (port_path.is_symlink(), std::fs::read_link(&port_path)) {
        if link_target.is_relative() {
            // A broken relative link has nothing to open, so keep the path the
            // user typed. canonicalize follows a chain when the final target exists.
            let joined = port_path.parent().unwrap().join(link_target);
            if let Ok(resolved) = joined.canonicalize() {
                return resolved.to_string_lossy().to_string();
            }
        } else if let Ok(resolved) = link_target.canonicalize() {
            return resolved.to_string_lossy().to_string();
        } else {
            // An absolute link to a missing target returns that target.
            return link_target.to_string_lossy().to_string();
        }
    }
    port_name.to_owned()
}

fn filtered_ports(opt: &Opt) -> Result<Vec<SerialPortInfo>> {
    select_ports(available_ports()?, opt)
}

fn select_ports(ports: Vec<SerialPortInfo>, opt: &Opt) -> Result<Vec<SerialPortInfo>> {
    let mut ports: Vec<SerialPortInfo> = ports
        .into_iter()
        .filter(|info| usb_port_matches(info, opt))
        .collect();
    ports.sort_by(|a, b| natural_cmp(&a.port_name, &b.port_name));
    if let Some(index) = opt.index {
        if index < ports.len() {
            Ok(vec![ports[index].clone()])
        } else {
            Err(ProgramError::NoPortFound)
        }
    } else if ports.is_empty() {
        Err(ProgramError::NoPortFound)
    } else {
        Ok(ports)
    }
}

fn filtered_port(opt: &Opt) -> Result<SerialPortInfo> {
    Ok(filtered_ports(opt)?[0].clone())
}

// Formats the USB Port information into a human readable form.
fn extra_usb_info(info: &UsbPortInfo) -> String {
    let mut output = String::new();
    output = output + &format!(" {:04x}:{:04x}", info.vid, info.pid);
    let mut extra_items = Vec::new();

    if let Some(manufacturer) = &info.manufacturer {
        extra_items.push(format!("manufacturer '{}'", manufacturer));
    }
    if let Some(serial) = &info.serial_number {
        extra_items.push(format!("serial '{}'", serial));
    }
    if let Some(product) = &info.product {
        extra_items.push(format!("product '{}'", product));
    }
    if !extra_items.is_empty() {
        output += " with ";
        output += &extra_items.join(" ");
    }
    output
}

fn port_line(port: &SerialPortInfo) -> String {
    match &port.port_type {
        SerialPortType::UsbPort(info) => format!(
            "USB Serial Device{} found @{}",
            extra_usb_info(info),
            port.port_name
        ),
        _ => format!("Serial Device found @{}", port.port_name),
    }
}

// Lists all of the USB serial ports which match the filtering criteria.
fn list_ports(opt: &Opt) -> Result<()> {
    for port in filtered_ports(opt)? {
        println!("{}", port_line(&port));
    }
    Ok(())
}

// Returns the first port which matches the filtering criteria.
fn find_port(opt: &Opt) -> Result<String> {
    Ok(filtered_port(opt)?.port_name)
}

// Compare port names so trailing numbers sort by value: COM1, COM2, COM10.
fn natural_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let mut left_chars = left.chars().peekable();
    let mut right_chars = right.chars().peekable();
    loop {
        match (left_chars.peek().copied(), right_chars.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(left_ch), Some(right_ch))
                if left_ch.is_ascii_digit() && right_ch.is_ascii_digit() =>
            {
                let left_run = take_digits(&mut left_chars);
                let right_run = take_digits(&mut right_chars);
                match cmp_digit_run(&left_run, &right_run) {
                    std::cmp::Ordering::Equal => {}
                    order => return order,
                }
            }
            (Some(left_ch), Some(right_ch)) => match left_ch.cmp(&right_ch) {
                std::cmp::Ordering::Equal => {
                    left_chars.next();
                    right_chars.next();
                }
                order => return order,
            },
        }
    }
}

fn take_digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut digits = String::new();
    while let Some(ch) = chars.peek().copied() {
        if !ch.is_ascii_digit() {
            break;
        }
        digits.push(ch);
        chars.next();
    }
    digits
}

fn cmp_digit_run(left: &str, right: &str) -> std::cmp::Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    let left = if left.is_empty() { "0" } else { left };
    let right = if right.is_empty() { "0" } else { right };
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

// The byte produced by Ctrl together with an ASCII key, when that key is a
// control. Digits other than 4 through 7 are not controls: crossterm uses
// those four to report 0x1c through 0x1f.
fn control_byte(ch: char) -> Option<u8> {
    if ch.is_ascii_alphabetic() || ch == ' ' || ('@'..='_').contains(&ch) {
        Some((ch as u8) & 0x1f)
    } else if ('4'..='7').contains(&ch) {
        Some(((ch as u8) + 8) & 0x1f)
    } else {
        None
    }
}

// Converts key events from crossterm into appropriate character/escape sequences which are then
// sent over the serial connection.
fn handle_key_event(key_event: KeyEvent, opt: &Opt) -> Result<Option<Bytes>> {
    if opt.debug {
        println!("Event::{:?}\r", key_event);
    }
    // Windows crossterm delivers Press and Release for every key. The release
    // is not another press.
    if key_event.kind == KeyEventKind::Release {
        return Ok(None);
    }

    // The following escape sequeces come from the MicroPython codebase.
    //
    //  Up      ESC [A
    //  Down    ESC [B
    //  Right   ESC [C
    //  Left    ESC [D
    //  Home    ESC [H  or ESC [1~
    //  End     ESC [F  or ESC [4~
    //  Del     ESC [3~
    //  Insert  ESC [2~

    let mut buf = [0; 4];

    let key_str: Option<&[u8]> = match key_event.code {
        KeyCode::Backspace => Some(b"\x08"),
        KeyCode::Enter => Some(opt.enter.bytes()),
        KeyCode::Left => Some(b"\x1b[D"),
        KeyCode::Right => Some(b"\x1b[C"),
        KeyCode::Home => Some(b"\x1b[H"),
        KeyCode::End => Some(b"\x1b[F"),
        KeyCode::Up => Some(b"\x1b[A"),
        KeyCode::Down => Some(b"\x1b[B"),
        KeyCode::Tab => Some(b"\x09"),
        KeyCode::Delete => Some(b"\x1b[3~"),
        KeyCode::Insert => Some(b"\x1b[2~"),
        KeyCode::Esc => Some(b"\x1b"),
        KeyCode::Char(ch) => {
            if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                if let Some(byte) = control_byte(ch) {
                    buf[0] = byte;
                    Some(&buf[0..1])
                } else {
                    Some(ch.encode_utf8(&mut buf).as_bytes())
                }
            } else {
                Some(ch.encode_utf8(&mut buf).as_bytes())
            }
        }
        _ => None,
    };
    if let Some(key_str) = key_str {
        if opt.debug {
            println!("Send: {}\r", hex_str(key_str));
        }
        if opt.echo {
            if let Ok(val) = std::str::from_utf8(key_str) {
                print!("{}", val);
                std::io::stdout().flush()?;
            }
        }
        Ok(Some(Bytes::copy_from_slice(key_str)))
    } else {
        Ok(None)
    }
}

#[derive(Debug)]
enum TerminalAction {
    Exit,
    Send(Bytes),
    Ignore,
    Unrecognized,
}

fn terminal_action(event: &Event, opt: &Opt) -> Result<TerminalAction> {
    if is_exit_event(event, opt) {
        return Ok(TerminalAction::Exit);
    }
    match event {
        Event::Key(key_event) => {
            if let Some(key) = handle_key_event(*key_event, opt)? {
                Ok(TerminalAction::Send(key))
            } else {
                Ok(TerminalAction::Ignore)
            }
        }
        Event::Resize(_, _) => Ok(TerminalAction::Ignore),
        _ => Ok(TerminalAction::Unrecognized),
    }
}

// Main function which collects input from the user and sends it over the serial link
// and collects serial data and presents it to the user.
async fn monitor(port: &mut tokio_serial::SerialStream, opt: &Opt) -> Result<()> {
    let mut reader = EventStream::new();
    let (rx_port, tx_port) = tokio::io::split(port);

    let mut serial_reader = tokio_util::codec::FramedRead::new(rx_port, StringDecoder::new());
    let serial_sink = tokio_util::codec::FramedWrite::new(tx_port, BytesCodec::new());
    let (serial_writer, serial_consumer) = futures::channel::mpsc::unbounded::<Bytes>();

    let mut poll_send = serial_consumer.map(Ok).forward(serial_sink);
    loop {
        let mut event = reader.next().fuse();
        let mut serial_event = serial_reader.next().fuse();

        select! {
            _ = poll_send => {}
            maybe_event = event => {
                match maybe_event {
                    Some(Ok(event)) => match terminal_action(&event, opt)? {
                        TerminalAction::Exit => break,
                        TerminalAction::Send(key) => {
                            serial_writer.unbounded_send(key).unwrap();
                        }
                        TerminalAction::Ignore => {}
                        TerminalAction::Unrecognized => {
                            println!("Unrecognized Event::{:?}\r", event);
                        }
                    },
                    Some(Err(e)) => println!("crossterm Error: {:?}\r", e),
                    None => {
                        println!("maybe_event returned None\r");
                    },
                }
            },
            maybe_serial = serial_event => {
                match maybe_serial {
                    Some(Ok(serial_event)) => {
                        if opt.debug {
                            println!("Serial Event:{:?}\r", serial_event);
                        } else {
                            print!("{}", serial_event);
                            std::io::stdout().flush()?;
                        }
                    },
                    Some(Err(e)) => {
                        println!("Serial Error: {:?}\r", e);
                        // This most likely means that the serial port has been unplugged.
                        break;
                    },
                    None => {
                        println!("maybe_serial returned None\r");
                    },
                }
            },
        };
    }

    Ok(())
}

struct ExitReport {
    code: i32,
    message: Option<String>,
}

fn exit_report(result: &Result<()>) -> ExitReport {
    match result {
        Ok(()) => ExitReport {
            code: 0,
            message: None,
        },
        Err(ProgramError::NoPortFound) => ExitReport {
            code: 1,
            message: Some("No USB serial ports found".to_string()),
        },
        Err(err) => ExitReport {
            code: 2,
            message: Some(format!("Error: {:?}", err)),
        },
    }
}

// Main entry point to the program.
#[tokio::main]
async fn main() -> Result<()> {
    let result = real_main().await;
    let report = exit_report(&result);
    if let Some(message) = report.message {
        writeln!(&mut std::io::stderr(), "{}", message)?;
    }
    std::process::exit(report.code);
}

enum StartupAction {
    List,
    Find,
    Monitor,
}

fn startup_action(opt: &Opt) -> StartupAction {
    if opt.list {
        StartupAction::List
    } else if opt.find {
        StartupAction::Find
    } else {
        StartupAction::Monitor
    }
}

#[derive(Debug)]
struct PortSettings {
    baud: u32,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow_control: FlowControl,
}

fn port_settings(opt: &Opt) -> Result<PortSettings> {
    Ok(PortSettings {
        baud: opt.baud,
        data_bits: DataBitsOpt::try_from(opt.databits)?.0,
        parity: opt.parity.into(),
        stop_bits: StopBitsOpt::try_from(opt.stopbits)?.0,
        flow_control: opt.flow.into(),
    })
}

async fn real_main() -> Result<()> {
    let opt = Opt::from_args();

    if opt.verbose {
        println!("{:#?}", opt);
    }

    match startup_action(&opt) {
        StartupAction::List => {
            list_ports(&opt)?;
            return Ok(());
        }
        StartupAction::Find => {
            println!("{}", find_port(&opt)?);
            return Ok(());
        }
        StartupAction::Monitor => {}
    }

    let port_name = find_port(&opt)?;
    let settings = port_settings(&opt)?;

    // Do the serial port monitoring
    let port_builder = tokio_serial::new(&port_name, settings.baud)
        .data_bits(settings.data_bits)
        .parity(settings.parity)
        .stop_bits(settings.stop_bits)
        .flow_control(settings.flow_control);

    let err_port_name = port_name.clone();
    let mut port = tokio_serial::SerialStream::open(&port_builder)
        .map_err(|e| ProgramError::UnableToOpen(err_port_name, e.into()))?;

    println!("Connected to {}", port_name);
    println!("Press {} to exit", exit_label(&opt));
    enable_raw_mode()?;
    let result = monitor(&mut port, &opt).await;
    disable_raw_mode()?;
    println!();
    result
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::get_port_path;
    use super::{
        exit_label, exit_report, handle_key_event, hex_str, is_exit_event, map_port_name,
        port_line, port_settings, select_ports, startup_action, terminal_action, usb_port_matches,
        Eol, FlowControlOpt, Opt, ParityOpt, ProgramError, StartupAction, TerminalAction,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use mio_serial::SerialPortInfo;
    use serialport::{SerialPortType, UsbPortInfo};
    use structopt::StructOpt;
    use tokio_serial::{DataBits, FlowControl, Parity, StopBits};

    // These tests specify the behavior we want. Some fail until the program
    // matches that behavior. Do not change an assertion to match the current code.

    fn bare() -> Opt {
        Opt {
            port: None,
            baud: 115200,
            debug: false,
            echo: false,
            list: false,
            enter: Eol::Cr,
            find: false,
            verbose: false,
            ctrl_y_exit: false,
            vid: None,
            pid: None,
            manufacturer: None,
            serial: None,
            product: None,
            index: None,
            parity: ParityOpt::None,
            stopbits: 1,
            flow: FlowControlOpt::None,
            databits: 8,
        }
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Press)
    }

    fn sent(event: KeyEvent, opt: &Opt) -> Option<Vec<u8>> {
        handle_key_event(event, opt)
            .unwrap()
            .map(|bytes| bytes.to_vec())
    }

    fn usb(name: &str, vid: u16, pid: u16) -> SerialPortInfo {
        named_usb(name, vid, pid, None, None, None)
    }

    fn named_usb(
        name: &str,
        vid: u16,
        pid: u16,
        manufacturer: Option<&str>,
        product: Option<&str>,
        serial: Option<&str>,
    ) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid,
                pid,
                serial_number: serial.map(str::to_string),
                manufacturer: manufacturer.map(str::to_string),
                product: product.map(str::to_string),
            }),
        }
    }

    fn other_port(name: &str, port_type: SerialPortType) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type,
        }
    }

    #[test]
    fn printable_and_navigation_keys_send_their_bytes() {
        let opt = bare();
        let cases = [
            (KeyCode::Char('a'), &b"a"[..]),
            (KeyCode::Char('A'), b"A"),
            (KeyCode::Backspace, b"\x08"),
            (KeyCode::Tab, b"\x09"),
            (KeyCode::Esc, b"\x1b"),
            (KeyCode::Left, b"\x1b[D"),
            (KeyCode::Right, b"\x1b[C"),
            (KeyCode::Up, b"\x1b[A"),
            (KeyCode::Down, b"\x1b[B"),
            (KeyCode::Home, b"\x1b[H"),
            (KeyCode::End, b"\x1b[F"),
            (KeyCode::Delete, b"\x1b[3~"),
            (KeyCode::Insert, b"\x1b[2~"),
        ];
        for (code, bytes) in cases {
            assert_eq!(
                sent(press(code, KeyModifiers::NONE), &opt).as_deref(),
                Some(bytes),
                "{code:?}"
            );
        }
    }

    #[test]
    fn enter_follows_the_eol_setting() {
        let mut opt = bare();
        let enter = press(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(sent(enter, &opt).as_deref(), Some(&b"\r"[..]));
        opt.enter = Eol::Lf;
        assert_eq!(sent(enter, &opt).as_deref(), Some(&b"\n"[..]));
        opt.enter = Eol::Crlf;
        assert_eq!(sent(enter, &opt).as_deref(), Some(&b"\r\n"[..]));
    }

    #[test]
    fn lowercase_ctrl_keys_become_control_bytes() {
        let opt = bare();
        let cases = [
            ('a', 0x01u8),
            ('z', 0x1a),
            (' ', 0x00),
            ('4', 0x1c),
            ('5', 0x1d),
            ('6', 0x1e),
            ('7', 0x1f),
        ];
        for (ch, byte) in cases {
            let event = press(KeyCode::Char(ch), KeyModifiers::CONTROL);
            assert_eq!(sent(event, &opt).as_deref(), Some(&[byte][..]), "{ch:?}");
        }
    }

    #[test]
    fn ctrl_uppercase_and_punctuation_become_control_bytes() {
        let opt = bare();
        let cases = [('A', 0x01u8), ('X', 0x18), ('@', 0x00), ('[', 0x1b)];
        let mut missed = Vec::new();
        for (ch, byte) in cases {
            let event = press(KeyCode::Char(ch), KeyModifiers::CONTROL);
            let got = sent(event, &opt);
            if got.as_deref() != Some(&[byte][..]) {
                missed.push((ch, got));
            }
        }
        assert!(missed.is_empty(), "{:?}", missed);
    }

    #[test]
    fn repeat_sends_the_key_again() {
        let opt = bare();
        let event =
            KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(sent(event, &opt).as_deref(), Some(&b"a"[..]));
        let ctrl = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        );
        assert_eq!(sent(ctrl, &opt).as_deref(), Some(&[0x01][..]));
    }

    #[test]
    fn key_release_sends_nothing() {
        let opt = bare();
        let events = [
            KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release),
            KeyEvent::new_with_kind(KeyCode::Left, KeyModifiers::NONE, KeyEventKind::Release),
            KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                KeyEventKind::Release,
            ),
        ];
        let mut sent_anyway = Vec::new();
        for event in events {
            if sent(event, &opt).is_some() {
                sent_anyway.push(event);
            }
        }
        assert!(sent_anyway.is_empty(), "{:?}", sent_anyway);
    }

    fn ctrl_key(ch: char, kind: KeyEventKind, state: KeyEventState) -> Event {
        Event::Key(KeyEvent::new_with_kind_and_state(
            KeyCode::Char(ch),
            KeyModifiers::CONTROL,
            kind,
            state,
        ))
    }

    #[test]
    fn exit_chord_is_ctrl_x_press_unless_y_is_selected() {
        let opt = bare();
        assert!(is_exit_event(
            &ctrl_key('x', KeyEventKind::Press, KeyEventState::NONE),
            &opt
        ));
        assert!(!is_exit_event(
            &ctrl_key('x', KeyEventKind::Release, KeyEventState::NONE),
            &opt
        ));
        assert!(!is_exit_event(
            &ctrl_key('y', KeyEventKind::Press, KeyEventState::NONE),
            &opt
        ));

        let mut opt = bare();
        opt.ctrl_y_exit = true;
        assert!(is_exit_event(
            &ctrl_key('y', KeyEventKind::Press, KeyEventState::NONE),
            &opt
        ));
        assert!(!is_exit_event(
            &ctrl_key('x', KeyEventKind::Press, KeyEventState::NONE),
            &opt
        ));
        assert_eq!(
            sent(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
                &opt
            )
            .as_deref(),
            Some(&[0x18][..])
        );
    }

    #[test]
    fn exit_chord_ignores_case_and_lock_state() {
        let mut missed = Vec::new();
        let cases = [
            ("uppercase X", 'X', false, KeyEventState::NONE),
            ("caps lock", 'x', false, KeyEventState::CAPS_LOCK),
            ("uppercase Y with -y", 'Y', true, KeyEventState::NONE),
            ("caps lock Y with -y", 'y', true, KeyEventState::CAPS_LOCK),
        ];
        for (label, ch, ctrl_y, state) in cases {
            let mut opt = bare();
            opt.ctrl_y_exit = ctrl_y;
            if !is_exit_event(&ctrl_key(ch, KeyEventKind::Press, state), &opt) {
                missed.push(label);
            }
        }
        assert!(missed.is_empty(), "{:?}", missed);
    }

    #[test]
    fn product_substring_match_without_a_wildcard() {
        let port = named_usb(
            "COM4",
            0xf055,
            0x9800,
            None,
            Some("Pyboard Virtual Comm Port in FS Mode"),
            None,
        );
        let mut opt = bare();
        opt.product = Some("FS".to_string());
        assert!(usb_port_matches(&port, &opt));
    }

    #[test]
    fn port_wildcard_matches_a_single_character() {
        let com1 = usb("COM1", 1, 2);
        let com10 = usb("COM10", 1, 2);
        let mut opt = bare();
        opt.port = Some("COM?".to_string());
        assert!(usb_port_matches(&com1, &opt));
        assert!(!usb_port_matches(&com10, &opt));
    }

    #[test]
    fn port_name_without_a_wildcard_is_exact() {
        let com1 = usb("COM1", 1, 2);
        let com10 = usb("COM10", 1, 2);
        let mut opt = bare();
        opt.port = Some("COM1".to_string());
        assert!(usb_port_matches(&com1, &opt));
        assert!(!usb_port_matches(&com10, &opt));
    }

    #[test]
    fn missing_usb_string_does_not_match_a_text_filter() {
        let port = usb("COM4", 0x0403, 0x6001);
        let mut opt = bare();
        opt.manufacturer = Some("FTDI".to_string());
        assert!(!usb_port_matches(&port, &opt));
    }

    #[test]
    fn unfiltered_list_keeps_non_usb_ports_hidden() {
        let usb_port = usb("COM4", 1, 2);
        let pci = other_port("COM1", SerialPortType::PciPort);
        let opt = bare();
        assert!(usb_port_matches(&usb_port, &opt));
        assert!(!usb_port_matches(&pci, &opt));
    }

    #[test]
    fn usb_id_filter_ignores_ports_without_usb_ids() {
        let pci = other_port("COM1", SerialPortType::PciPort);
        let mut opt = bare();
        opt.vid = Some("f055".to_string());
        assert!(!usb_port_matches(&pci, &opt));
    }

    #[test]
    fn id_filters_accept_case_and_0x_prefix() {
        let port = usb("COM4", 0xf055, 0x9800);
        let mut missed = Vec::new();
        for pattern in ["F055", "0xf055", "0xF055"] {
            let mut opt = bare();
            opt.vid = Some(pattern.to_string());
            if !usb_port_matches(&port, &opt) {
                missed.push(pattern);
            }
        }
        let mut opt = bare();
        opt.pid = Some("0x9800".to_string());
        if !usb_port_matches(&port, &opt) {
            missed.push("pid 0x9800");
        }
        assert!(missed.is_empty(), "{:?}", missed);
    }

    #[test]
    fn lowercase_hex_id_matches() {
        let port = usb("COM4", 0xf055, 0x9800);
        let mut opt = bare();
        opt.vid = Some("f055".to_string());
        assert!(usb_port_matches(&port, &opt));
        opt.vid = None;
        opt.pid = Some("9800".to_string());
        assert!(usb_port_matches(&port, &opt));
    }

    #[test]
    fn id_filters_are_not_hex_substrings() {
        let port = usb("COM4", 0xf055, 0x9800);
        let mut opt = bare();
        opt.vid = Some("055".to_string());
        assert!(!usb_port_matches(&port, &opt));
        opt.vid = None;
        opt.pid = Some("800".to_string());
        assert!(!usb_port_matches(&port, &opt));
    }

    #[test]
    fn text_filters_are_case_insensitive() {
        let port = named_usb(
            "COM4",
            0x0403,
            0x6001,
            Some("FTDI"),
            Some("FT232R USB UART"),
            Some("A700e6Lr"),
        );
        let mut missed = Vec::new();
        let mut manufacturer = bare();
        manufacturer.manufacturer = Some("ftdi".to_string());
        if !usb_port_matches(&port, &manufacturer) {
            missed.push("manufacturer");
        }
        let mut product = bare();
        product.product = Some("ft232r".to_string());
        if !usb_port_matches(&port, &product) {
            missed.push("product");
        }
        let mut serial = bare();
        serial.serial = Some("a700e6lr".to_string());
        if !usb_port_matches(&port, &serial) {
            missed.push("serial");
        }
        assert!(missed.is_empty(), "{:?}", missed);
    }

    #[test]
    fn port_filter_matches_non_usb_ports_by_name() {
        let mut opt = bare();
        opt.port = Some("COM3".to_string());
        let kinds = [
            SerialPortType::PciPort,
            SerialPortType::BluetoothPort,
            SerialPortType::Unknown,
        ];
        let mut missed = Vec::new();
        for kind in kinds {
            if !usb_port_matches(&other_port("COM3", kind.clone()), &opt) {
                missed.push(kind);
            }
        }
        assert!(missed.is_empty(), "{:?}", missed);
    }

    #[test]
    fn index_uses_natural_port_order() {
        assert_natural_order(&["COM10", "COM2", "COM1"], &["COM1", "COM2", "COM10"]);
        assert_natural_order(
            &["/dev/ttyUSB10", "/dev/ttyUSB2"],
            &["/dev/ttyUSB2", "/dev/ttyUSB10"],
        );
    }

    fn assert_natural_order(names: &[&str], expect: &[&str]) {
        let ports: Vec<_> = names.iter().map(|name| usb(name, 1, 1)).collect();
        for (index, name) in expect.iter().enumerate() {
            let mut opt = bare();
            opt.index = Some(index);
            let got = select_ports(ports.clone(), &opt).unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].port_name, *name, "index {index}");
        }
    }

    #[test]
    fn no_ports_and_index_past_the_end_are_errors() {
        assert!(select_ports(Vec::new(), &bare()).is_err());
        let mut opt = bare();
        opt.index = Some(5);
        assert!(select_ports(vec![usb("COM1", 1, 1)], &opt).is_err());
    }

    #[cfg(unix)]
    fn temp_workspace(label: &str) -> (std::path::PathBuf, RmDir) {
        let dir =
            std::env::temp_dir().join(format!("serial-monitor-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), RmDir(dir))
    }

    #[cfg(unix)]
    struct RmDir(std::path::PathBuf);

    #[cfg(unix)]
    impl Drop for RmDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn plain_path_is_unchanged() {
        let (dir, _rm) = temp_workspace("plain");
        let device = dir.join("device");
        std::fs::write(&device, b"").unwrap();
        let name = device.to_str().unwrap();
        assert_eq!(get_port_path(name), name);
    }

    #[cfg(unix)]
    #[test]
    fn relative_symlink_resolves_to_its_target() {
        let (dir, _rm) = temp_workspace("rel");
        let device = dir.join("device");
        std::fs::write(&device, b"").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink("device", &link).unwrap();
        let got = get_port_path(link.to_str().unwrap());
        assert_eq!(std::path::Path::new(&got), device.canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn absolute_symlink_chain_resolves_to_the_final_path() {
        let (dir, _rm) = temp_workspace("chain");
        let device = dir.join("device");
        std::fs::write(&device, b"").unwrap();
        let mid = dir.join("mid");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&device, &mid).unwrap();
        std::os::unix::fs::symlink(&mid, &link).unwrap();
        let got = get_port_path(link.to_str().unwrap());
        assert_eq!(std::path::Path::new(&got), device.canonicalize().unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tty_path_matches_enumerated_cu_port() {
        let port = usb("/dev/cu.usbmodemX", 1, 2);
        let mut opt = bare();
        opt.port = Some("/dev/tty.usbmodemX".to_string());
        assert!(usb_port_matches(&port, &opt));
    }

    #[cfg(unix)]
    #[test]
    fn missing_path_is_unchanged() {
        let (dir, _rm) = temp_workspace("missing");
        let missing = dir.join("missing");
        let name = missing.to_str().unwrap();
        assert_eq!(get_port_path(name), name);
    }

    #[cfg(unix)]
    #[test]
    fn absolute_symlink_to_a_missing_target_returns_that_target() {
        let (dir, _rm) = temp_workspace("dangling");
        let missing = dir.join("missing");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&missing, &link).unwrap();
        assert_eq!(
            get_port_path(link.to_str().unwrap()),
            missing.to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_relative_symlink_keeps_the_link_path() {
        let (dir, _rm) = temp_workspace("broken-rel");
        let link = dir.join("link");
        std::os::unix::fs::symlink("missing", &link).unwrap();
        let name = link.to_str().unwrap();
        assert_eq!(get_port_path(name), name);
    }

    #[test]
    fn exit_label_names_the_chord() {
        assert_eq!(exit_label(&bare()), "Control-X");
        let mut opt = bare();
        opt.ctrl_y_exit = true;
        assert_eq!(exit_label(&opt), "Control-Y");
    }

    #[test]
    fn hex_str_formats_text_controls_and_high_bytes() {
        assert_eq!(hex_str(b""), ": ");
        assert_eq!(hex_str(b"A"), "41 : A");
        assert_eq!(hex_str(b" "), "20 :  ");
        assert_eq!(hex_str(b"\x00"), "00 : Ctrl-@");
        assert_eq!(hex_str(b"\x01"), "01 : Ctrl-A");
        assert_eq!(hex_str(b"\x1b"), "1b : ESC");
        assert_eq!(hex_str(b"\x1f"), "1f : Ctrl-_");
        assert_eq!(hex_str(b"\x7f"), "7f : .");
        assert_eq!(hex_str(b"\xff"), "ff : .");
        assert_eq!(hex_str(b"A\x1bB"), "41 1b 42 : A ESCB");
        assert_eq!(hex_str(b"\x01A"), "01 41 : Ctrl-AA");
        assert_eq!(hex_str(b"\x80\xfe"), "80 fe : ..");
    }

    #[test]
    fn port_lines_include_the_usb_fields_that_are_present() {
        let mut info = usb("COM5", 0x0403, 0x6001);
        assert_eq!(port_line(&info), "USB Serial Device 0403:6001 found @COM5");
        info = named_usb(
            "COM5",
            0x0403,
            0x6001,
            Some("FTDI"),
            Some("Thing"),
            Some("ABC"),
        );
        assert_eq!(
            port_line(&info),
            "USB Serial Device 0403:6001 with manufacturer 'FTDI' serial 'ABC' product 'Thing' found @COM5"
        );
        info = named_usb("COM1", 0x0001, 0x0002, None, Some("P"), None);
        assert_eq!(
            port_line(&info),
            "USB Serial Device 0001:0002 with product 'P' found @COM1"
        );
        assert_eq!(
            port_line(&other_port("COM1", SerialPortType::PciPort)),
            "Serial Device found @COM1"
        );
    }

    #[test]
    fn filters_are_conjunctive_and_explicit_wildcards_are_not_wrapped() {
        let port = named_usb(
            "COM4",
            0xf055,
            0x9800,
            Some("FTDI"),
            Some("Pyboard"),
            Some("A700"),
        );
        let mut opt = bare();
        opt.vid = Some("f055".to_string());
        opt.pid = Some("9800".to_string());
        opt.manufacturer = Some("FTDI".to_string());
        assert!(usb_port_matches(&port, &opt));
        opt.product = Some("Other".to_string());
        assert!(!usb_port_matches(&port, &opt));

        let com1 = usb("COM1", 1, 1);
        let com10 = usb("COM10", 1, 1);
        let com2 = usb("COM2", 1, 1);
        opt = bare();
        opt.port = Some("COM1*".to_string());
        assert!(usb_port_matches(&com1, &opt));
        assert!(usb_port_matches(&com10, &opt));
        assert!(!usb_port_matches(&com2, &opt));
        opt.port = Some("*COM1".to_string());
        assert!(usb_port_matches(&com1, &opt));
        assert!(!usb_port_matches(&com10, &opt));
    }

    #[test]
    fn text_filter_matches_the_middle_of_the_field() {
        let port = named_usb("COM4", 1, 2, None, None, Some("A700e6Lr"));
        let mut opt = bare();
        opt.serial = Some("700".to_string());
        assert!(usb_port_matches(&port, &opt));
    }

    #[test]
    fn no_index_returns_every_match() {
        let ports = vec![usb("COM2", 1, 1), usb("COM1", 1, 1)];
        let got = select_ports(ports, &bare()).unwrap();
        let mut names: Vec<_> = got.into_iter().map(|port| port.port_name).collect();
        names.sort();
        assert_eq!(names, ["COM1", "COM2"]);
    }

    #[test]
    fn one_matching_port_is_selected() {
        let got = select_ports(vec![usb("COM4", 1, 1)], &bare()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].port_name, "COM4");
    }

    #[test]
    fn a_filter_that_excludes_every_port_is_an_error() {
        let mut opt = bare();
        opt.port = Some("COM9".to_string());
        assert!(select_ports(vec![usb("COM1", 1, 1)], &opt).is_err());
    }

    #[test]
    fn mac_tty_names_are_rewritten_to_cu_names() {
        assert_eq!(map_port_name("/dev/tty.usbmodem123"), "/dev/cu.usbmodem123");
        assert_eq!(map_port_name("/dev/cu.usbmodem123"), "/dev/cu.usbmodem123");
        assert_eq!(map_port_name("/dev/ttyUSB0"), "/dev/ttyUSB0");
        assert_eq!(map_port_name("COM3"), "COM3");
    }

    #[test]
    fn default_line_settings_are_8n1_at_115200() {
        let settings = port_settings(&bare()).unwrap();
        assert_eq!(settings.baud, 115200);
        assert_eq!(settings.data_bits, DataBits::Eight);
        assert_eq!(settings.parity, Parity::None);
        assert_eq!(settings.stop_bits, StopBits::One);
        assert_eq!(settings.flow_control, FlowControl::None);
    }

    #[test]
    fn line_settings_accept_every_supported_value() {
        for (bits, data_bits) in [
            (5, DataBits::Five),
            (6, DataBits::Six),
            (7, DataBits::Seven),
            (8, DataBits::Eight),
        ] {
            let mut opt = bare();
            opt.databits = bits;
            assert_eq!(port_settings(&opt).unwrap().data_bits, data_bits);
        }
        for (bits, stop_bits) in [(1, StopBits::One), (2, StopBits::Two)] {
            let mut opt = bare();
            opt.stopbits = bits;
            assert_eq!(port_settings(&opt).unwrap().stop_bits, stop_bits);
        }
        for (parity, expect) in [
            (ParityOpt::None, Parity::None),
            (ParityOpt::Odd, Parity::Odd),
            (ParityOpt::Even, Parity::Even),
        ] {
            let mut opt = bare();
            opt.parity = parity;
            assert_eq!(port_settings(&opt).unwrap().parity, expect);
        }
        for (flow, expect) in [
            (FlowControlOpt::None, FlowControl::None),
            (FlowControlOpt::Software, FlowControl::Software),
            (FlowControlOpt::Hardware, FlowControl::Hardware),
        ] {
            let mut opt = bare();
            opt.flow = flow;
            assert_eq!(port_settings(&opt).unwrap().flow_control, expect);
        }
    }

    #[test]
    fn line_settings_reject_unsupported_values() {
        for databits in [0, 4, 9] {
            let mut opt = bare();
            opt.databits = databits;
            let err = port_settings(&opt).unwrap_err();
            assert_eq!(err.to_string(), "databits out of range");
        }
        for stopbits in [0, 3] {
            let mut opt = bare();
            opt.stopbits = stopbits;
            let err = port_settings(&opt).unwrap_err();
            assert_eq!(err.to_string(), "stopbits out of range");
        }
    }

    #[test]
    fn startup_action_lets_list_win_over_find() {
        let mut opt = bare();
        assert!(matches!(startup_action(&opt), StartupAction::Monitor));
        opt.find = true;
        assert!(matches!(startup_action(&opt), StartupAction::Find));
        opt.list = true;
        assert!(matches!(startup_action(&opt), StartupAction::List));
    }

    #[test]
    fn command_line_defaults_and_overrides() {
        let opt = Opt::from_iter_safe(["serial-monitor"]).unwrap();
        assert_eq!(opt.baud, 115200);
        assert_eq!(opt.databits, 8);
        assert_eq!(opt.stopbits, 1);
        assert_eq!(opt.enter.bytes(), b"\r");
        assert!(!opt.list && !opt.find && !opt.echo && !opt.debug && !opt.verbose);
        assert!(!opt.ctrl_y_exit);
        assert!(opt.port.is_none() && opt.vid.is_none() && opt.index.is_none());

        let opt = Opt::from_iter_safe([
            "serial-monitor",
            "--port",
            "COM4",
            "--baud",
            "9600",
            "--databits",
            "7",
            "--stopbits",
            "2",
            "--parity",
            "odd",
            "--flow",
            "hardware",
            "--enter",
            "lf",
            "--vid",
            "f055",
            "--pid",
            "9800",
            "--manufacturer",
            "FTDI",
            "--serial",
            "ABC",
            "--product",
            "Pyboard",
            "--index",
            "2",
            "-e",
            "-d",
            "-v",
            "-y",
            "-l",
        ])
        .unwrap();
        assert_eq!(opt.port.as_deref(), Some("COM4"));
        assert_eq!(opt.baud, 9600);
        assert_eq!(opt.databits, 7);
        assert_eq!(opt.stopbits, 2);
        assert!(matches!(opt.parity, ParityOpt::Odd));
        assert!(matches!(opt.flow, FlowControlOpt::Hardware));
        assert_eq!(opt.enter.bytes(), b"\n");
        assert_eq!(opt.vid.as_deref(), Some("f055"));
        assert_eq!(opt.pid.as_deref(), Some("9800"));
        assert_eq!(opt.manufacturer.as_deref(), Some("FTDI"));
        assert_eq!(opt.serial.as_deref(), Some("ABC"));
        assert_eq!(opt.product.as_deref(), Some("Pyboard"));
        assert_eq!(opt.index, Some(2));
        assert!(opt.echo && opt.debug && opt.verbose && opt.ctrl_y_exit && opt.list);
        assert!(matches!(startup_action(&opt), StartupAction::List));
    }

    #[test]
    fn command_line_rejects_unknown_values() {
        assert!(Opt::from_iter_safe(["serial-monitor", "--enter", "nope"]).is_err());
        assert!(Opt::from_iter_safe(["serial-monitor", "--parity", "mark"]).is_err());
        assert!(Opt::from_iter_safe(["serial-monitor", "--flow", "rts"]).is_err());
        assert!(Opt::from_iter_safe(["serial-monitor", "--baud", "fast"]).is_err());
        assert!(Opt::from_iter_safe(["serial-monitor", "--not-a-flag"]).is_err());
    }

    #[test]
    fn exit_report_uses_zero_one_and_two() {
        let ok = exit_report(&Ok(()));
        assert_eq!(ok.code, 0);
        assert!(ok.message.is_none());

        let missing = exit_report(&Err(ProgramError::NoPortFound));
        assert_eq!(missing.code, 1);
        assert_eq!(
            missing.message.as_deref(),
            Some("No USB serial ports found")
        );

        let io_err = exit_report(&Err(ProgramError::IoError(std::io::Error::other("disk"))));
        assert_eq!(io_err.code, 2);
        assert_eq!(io_err.message.as_deref(), Some("Error: disk"));

        let open = exit_report(&Err(ProgramError::UnableToOpen(
            "COM1".to_string(),
            std::io::Error::other("busy"),
        )));
        assert_eq!(
            open.message.as_deref(),
            Some("Error: Unable to open serial port 'COM1': busy")
        );

        let serial = exit_report(&Err(ProgramError::SerialPortError(mio_serial::Error::new(
            mio_serial::ErrorKind::Unknown,
            "boom",
        ))));
        assert_eq!(
            serial.message.as_deref(),
            Some("Error: SerialPortError: boom")
        );
    }

    #[test]
    fn terminal_action_exits_ignores_resize_and_sends_keys() {
        let opt = bare();
        let exit = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(matches!(
            terminal_action(&exit, &opt).unwrap(),
            TerminalAction::Exit
        ));

        let typed = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        match terminal_action(&typed, &opt).unwrap() {
            TerminalAction::Send(bytes) => assert_eq!(bytes.as_ref(), b"a"),
            other => panic!("expected send, got {:?}", other),
        }

        assert!(matches!(
            terminal_action(&Event::Resize(80, 24), &opt).unwrap(),
            TerminalAction::Ignore
        ));
        assert!(matches!(
            terminal_action(&Event::FocusGained, &opt).unwrap(),
            TerminalAction::Unrecognized
        ));
    }

    #[test]
    fn command_line_accepts_crlf_even_and_software_flow() {
        let opt = Opt::from_iter_safe([
            "serial-monitor",
            "--enter",
            "crlf",
            "--parity",
            "even",
            "--flow",
            "software",
        ])
        .unwrap();
        assert_eq!(opt.enter.bytes(), b"\r\n");
        assert!(matches!(opt.parity, ParityOpt::Even));
        assert!(matches!(opt.flow, FlowControlOpt::Software));
    }

    #[test]
    fn unicode_key_is_sent_as_utf8() {
        let event = press(KeyCode::Char('é'), KeyModifiers::NONE);
        assert_eq!(sent(event, &bare()).as_deref(), Some("é".as_bytes()));
        let ctrl = press(KeyCode::Char('é'), KeyModifiers::CONTROL);
        assert_eq!(sent(ctrl, &bare()).as_deref(), Some("é".as_bytes()));
    }

    #[test]
    fn terminal_action_sends_a_control_key_that_is_not_the_exit_chord() {
        let opt = bare();
        let ctrl_y = Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        match terminal_action(&ctrl_y, &opt).unwrap() {
            TerminalAction::Send(bytes) => assert_eq!(bytes.as_ref(), &[0x19]),
            other => panic!("expected send, got {:?}", other),
        }
    }

    #[test]
    fn port_names_are_case_sensitive() {
        let port = usb("COM1", 1, 1);
        let mut opt = bare();
        opt.port = Some("com1".to_string());
        assert!(!usb_port_matches(&port, &opt));
    }

    #[test]
    fn index_applies_after_the_filter() {
        let ports = vec![usb("COM2", 0x1111, 1), usb("COM1", 0xf055, 1)];
        let mut opt = bare();
        opt.vid = Some("f055".to_string());
        opt.index = Some(0);
        let got = select_ports(ports, &opt).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].port_name, "COM1");
    }

    #[test]
    fn vid_wildcard_is_case_insensitive() {
        let port = usb("COM4", 0xf055, 0x9800);
        let mut opt = bare();
        opt.vid = Some("F*".to_string());
        assert!(usb_port_matches(&port, &opt));
    }
}
