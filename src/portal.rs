// SoftAP configuration portal.
//
// When the user picks "Wi-Fi Setup" the station interface is torn down and the
// radio is reconfigured as an open access point on 192.168.4.1. This module
// then runs, on that AP's own network stack, the three things a phone needs
// before it will show you a web page:
//
//   * a DHCP server, so the phone gets an address at all (port 67),
//   * a DNS server that answers every A query with our own address, so
//     whatever captive-portal probe URL the phone tries resolves to us
//     (port 53),
//   * an HTTP server that answers *every* GET with the settings form, so that
//     probe gets HTML instead of the "you have internet" response it wanted
//     and the OS pops the captive-portal sheet automatically (port 80).
//
// The DHCP and DNS wire formats come from `edge-dhcp` / `edge-captive` with
// `default-features = false`: that drops their `edge-nal` transport layer
// (which is pinned to embassy-net 0.8 and would clash with the 0.9 this crate
// uses) and keeps only the pure codec, which we drive over embassy-net's own
// sockets here. The HTTP side is hand-rolled to match the rest of this
// codebase's request handling.
//
// Entering the portal is a one-way trip: the station interface is gone, so
// there is nothing to go back to. Every exit path reboots.

use alloc::{format, string::String, vec, vec::Vec};
use core::cell::Cell;
use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use edge_dhcp::{
    server::{Server as DhcpServer, ServerOptions},
    Options as DhcpOptions, Packet as DhcpPacket,
};
use embassy_futures::select::{select4, Either4};
use embassy_net::{
    udp::{PacketMetadata, UdpMetadata, UdpSocket},
    IpAddress, IpEndpoint, IpListenEndpoint, Stack,
};
use embassy_time::{Duration, Timer};
use esp_println::println;

use crate::settings::{Settings, FIELD_ORDER};
use crate::touch::TouchEvent;
use crate::ui::{self, UiState};

/// The AP's own address, which is also the gateway, the DNS server and the
/// address the user types into their browser.
pub const PORTAL_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
pub const PORTAL_SSID: &str = "voicebox-setup";
const PORTAL_URL: &str = "http://192.168.4.1";

/// Outcome of a portal session. Both variants end in a reboot; the difference
/// is only whether the new settings were written first.
pub enum PortalOutcome {
    /// The user submitted the form. Payload is the settings to persist.
    Saved(Settings),
    /// The user tapped the screen to back out without saving.
    Cancelled,
}

/// Runs the portal until the user either submits the form or taps to cancel.
///
/// `stack` must be the access-point interface's stack, already configured with
/// a static address of [`PORTAL_IP`], and the radio must already be in AP mode
/// - this function does not touch the `WifiController`.
#[allow(clippy::too_many_arguments)]
pub async fn run<DI, RST>(
    stack: Stack<'static>,
    display: &mut lcd_async::Display<DI, lcd_async::models::GC9A01, RST>,
    ui_frame: &mut [u8],
    touch_events: &embassy_sync::channel::Channel<
        embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
        TouchEvent,
        8,
    >,
    current: &Settings,
) -> PortalOutcome
where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    // All of these live on the heap, i.e. in PSRAM. That is safe here in a way
    // it would not be for DMA: embassy-net only ever touches socket buffers
    // with plain CPU loads and stores, and the one window where PSRAM is
    // unreachable (esp-storage disabling the cache for a flash write) is
    // wrapped in a critical section, so no packet handling can run inside it.
    // Keeping them off `.bss` matters because internal RAM is the scarce
    // resource on this board and the portal is a rare, short-lived mode.
    let mut dhcp_rx_meta = [PacketMetadata::EMPTY; 4];
    let mut dhcp_tx_meta = [PacketMetadata::EMPTY; 4];
    let mut dhcp_rx = vec![0u8; 1024];
    let mut dhcp_tx = vec![0u8; 1024];
    let mut dhcp_scratch = vec![0u8; 1024];

    let mut dns_rx_meta = [PacketMetadata::EMPTY; 4];
    let mut dns_tx_meta = [PacketMetadata::EMPTY; 4];
    let mut dns_rx = vec![0u8; 768];
    let mut dns_tx = vec![0u8; 768];
    let mut dns_scratch = vec![0u8; 768];

    let mut http_rx = vec![0u8; 2048];
    let mut http_tx = vec![0u8; 2048];

    // Shared only between futures on this single-threaded executor, so a Cell
    // is enough - there is no preemption point inside any of the accesses.
    let clients = Cell::new(0u8);

    let dhcp = dhcp_task(
        stack,
        &mut dhcp_rx_meta,
        &mut dhcp_rx,
        &mut dhcp_tx_meta,
        &mut dhcp_tx,
        &mut dhcp_scratch,
        &clients,
    );
    let dns = dns_task(
        stack,
        &mut dns_rx_meta,
        &mut dns_rx,
        &mut dns_tx_meta,
        &mut dns_tx,
        &mut dns_scratch,
    );
    let http = http_task(stack, &mut http_rx, &mut http_tx, current);
    let ui_loop = async {
        let mut anim = 0u32;
        loop {
            let state = UiState::Portal {
                ap_ssid: PORTAL_SSID.into(),
                url: PORTAL_URL.into(),
                clients: clients.get(),
            };
            ui::render(display, ui_frame, &state, anim).await;
            anim = anim.wrapping_add(1);
            // Re-render on a slow tick so the client counter appears, but bail
            // out the instant the user taps.
            if let Ok(TouchEvent::Tap { .. }) = embassy_time::with_timeout(
                Duration::from_millis(500),
                touch_events.receive(),
            )
            .await
            {
                return;
            }
        }
    };

    match select4(dhcp, dns, http, ui_loop).await {
        Either4::Third(settings) => PortalOutcome::Saved(settings),
        // The DHCP and DNS futures never return; a tap is the only other way out.
        _ => PortalOutcome::Cancelled,
    }
}

// ---------------------------------------------------------------- DHCP -----

#[allow(clippy::too_many_arguments)]
async fn dhcp_task(
    stack: Stack<'static>,
    rx_meta: &mut [PacketMetadata],
    rx_buf: &mut [u8],
    tx_meta: &mut [PacketMetadata],
    tx_buf: &mut [u8],
    scratch: &mut [u8],
    clients: &Cell<u8>,
) -> Settings {
    let mut socket = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    if let Err(e) = socket.bind(IpListenEndpoint { addr: None, port: 67 }) {
        println!("portal: dhcp bind failed: {e:?}");
        return core::future::pending().await;
    }

    let mut server = DhcpServer::<_, 4>::new(|| embassy_time::Instant::now().as_secs(), PORTAL_IP);
    let mut gw = [PORTAL_IP];
    let dns_servers = [PORTAL_IP];

    let mut packet = vec![0u8; 1024];
    loop {
        let (len, meta) = match socket.recv_from(&mut packet).await {
            Ok(v) => v,
            Err(e) => {
                println!("portal: dhcp recv error: {e:?}");
                continue;
            }
        };
        let request = match DhcpPacket::decode(&packet[..len]) {
            Ok(r) => r,
            Err(e) => {
                println!("portal: bad dhcp packet: {e:?}");
                continue;
            }
        };

        let mut options = ServerOptions::new(PORTAL_IP, Some(&mut gw));
        options.dns = &dns_servers;
        // RFC 8910. iOS and Android both read this and use it to open the
        // portal page directly instead of relying on a probe failing.
        options.captive_url = Some(PORTAL_URL);

        let mut opt_buf = DhcpOptions::buf();
        let Some(reply) = server.handle_request(&mut opt_buf, &options, &request) else {
            continue;
        };
        // True for both OFFER and ACK. The UI only distinguishes "some device
        // is talking to us" from "nothing has shown up yet", and a device that
        // got as far as an offer qualifies.
        let handed_out_address = !reply.yiaddr.is_unspecified();

        // Per RFC 2131 4.1: a relayed request goes back to the relay, a client
        // that already holds an address and didn't ask for a broadcast can be
        // unicast, and everything else has to be broadcast because the client
        // has no address to unicast to yet.
        let dest = if !request.giaddr.is_unspecified() {
            SocketAddr::V4(SocketAddrV4::new(request.giaddr, 67))
        } else if !request.ciaddr.is_unspecified() && !request.broadcast {
            SocketAddr::V4(SocketAddrV4::new(request.ciaddr, 68))
        } else {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, 68))
        };

        let encoded = match reply.encode(scratch) {
            Ok(b) => b,
            Err(e) => {
                println!("portal: dhcp encode failed: {e:?}");
                continue;
            }
        };
        let dest: UdpMetadata = match dest {
            SocketAddr::V4(v4) => IpEndpoint::new(
                IpAddress::Ipv4(*v4.ip()),
                v4.port(),
            )
            .into(),
            // The AP stack is IPv4-only; nothing else can reach this socket.
            _ => meta,
        };
        if let Err(e) = socket.send_to(encoded, dest).await {
            println!("portal: dhcp send error: {e:?}");
        } else if handed_out_address {
            clients.set(clients.get().saturating_add(1).min(9));
        }
    }
}

// ----------------------------------------------------------------- DNS -----

async fn dns_task(
    stack: Stack<'static>,
    rx_meta: &mut [PacketMetadata],
    rx_buf: &mut [u8],
    tx_meta: &mut [PacketMetadata],
    tx_buf: &mut [u8],
    scratch: &mut [u8],
) -> Settings {
    let mut socket = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    if let Err(e) = socket.bind(IpListenEndpoint { addr: None, port: 53 }) {
        println!("portal: dns bind failed: {e:?}");
        return core::future::pending().await;
    }

    let mut query = vec![0u8; 768];
    loop {
        let (len, meta) = match socket.recv_from(&mut query).await {
            Ok(v) => v,
            Err(e) => {
                println!("portal: dns recv error: {e:?}");
                continue;
            }
        };
        // Every name resolves to us. That is the whole point - it is what
        // turns the phone's connectivity check into a request we can answer.
        match edge_captive::reply(
            &query[..len],
            &PORTAL_IP.octets(),
            Duration::from_secs(60).into(),
            scratch,
        ) {
            Ok(n) => {
                if let Err(e) = socket.send_to(&scratch[..n], meta).await {
                    println!("portal: dns send error: {e:?}");
                }
            }
            Err(e) => println!("portal: dns reply failed: {e:?}"),
        }
    }
}

// ---------------------------------------------------------------- HTTP -----

/// Serves the form until a POST to `/save` arrives, then returns the settings
/// the user submitted.
async fn http_task(
    stack: Stack<'static>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    current: &Settings,
) -> Settings {
    loop {
        let mut socket = embassy_net::tcp::TcpSocket::new(stack, rx_buf, tx_buf);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if let Err(e) = socket.accept(IpListenEndpoint { addr: None, port: 80 }).await {
            println!("portal: accept failed: {e:?}");
            Timer::after(Duration::from_millis(200)).await;
            continue;
        }

        match serve_one(&mut socket, current).await {
            Some(new_settings) => {
                // Flush the "saved, rebooting" page before the caller reboots,
                // otherwise the phone shows a connection error instead.
                socket.close();
                Timer::after(Duration::from_millis(500)).await;
                socket.abort();
                return new_settings;
            }
            None => {
                socket.close();
                Timer::after(Duration::from_millis(50)).await;
                socket.abort();
            }
        }
        // `rx_buf`/`tx_buf` are reborrowed by the next iteration's socket, so
        // this one has to be fully dropped first - which it is, at the end of
        // the loop body.
    }
}

/// Handles a single request. Returns `Some` only for a successful form POST.
async fn serve_one(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    current: &Settings,
) -> Option<Settings> {
    // Read headers. 4 KiB is plenty for a request line plus a phone's headers;
    // a POST body that spills past them is read separately below.
    let mut buf = vec![0u8; 4096];
    let mut n = 0usize;
    let head_end = loop {
        if n == buf.len() {
            return None;
        }
        let read = socket.read(&mut buf[n..]).await.ok()?;
        if read == 0 {
            return None;
        }
        n += read;
        if let Some(p) = find(&buf[..n], b"\r\n\r\n") {
            break p + 4;
        }
    };

    let head = core::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?;
    let path = parts.next()?;
    println!("portal: {method} {path}");

    if method == "POST" && path.starts_with("/save") {
        let content_length = head
            .split("\r\n")
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if content_length > 8192 {
            respond(socket, "413 Payload Too Large", "text/plain", b"too big").await;
            return None;
        }

        let mut body = Vec::with_capacity(content_length);
        body.extend_from_slice(&buf[head_end..n]);
        while body.len() < content_length {
            let mut chunk = [0u8; 512];
            let read = socket.read(&mut chunk).await.ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(content_length);

        let updated = apply_form(current, &body);
        let page = saved_page();
        respond(socket, "200 OK", "text/html; charset=utf-8", page.as_bytes()).await;
        socket.flush().await.ok();
        return Some(updated);
    }

    // Anything else - `/`, `/generate_204`, `/hotspot-detect.html`, a favicon
    // request, whatever the phone probes with - gets the form. Handing HTML to
    // a connectivity probe is exactly what makes the OS decide it is behind a
    // captive portal and open the page on its own.
    let page = form_page(current);
    respond(socket, "200 OK", "text/html; charset=utf-8", page.as_bytes()).await;
    socket.flush().await.ok();
    None
}

async fn respond(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    status: &str,
    content_type: &str,
    body: &[u8],
) {
    use embedded_io_async::Write;
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    socket.write_all(body).await.ok();
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------- form -----

fn form_page(current: &Settings) -> String {
    let mut html = String::with_capacity(3072);
    html.push_str(
        "<!DOCTYPE html><html><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>Voicebox Setup</title><style>\
         body{font-family:-apple-system,system-ui,sans-serif;background:#111;color:#eee;\
         margin:0;padding:20px;max-width:520px}\
         h1{font-size:20px;margin:0 0 4px}p.sub{color:#888;font-size:13px;margin:0 0 20px}\
         label{display:block;margin:14px 0 4px;font-size:13px;color:#bbb}\
         input{width:100%;box-sizing:border-box;padding:10px;font-size:16px;\
         border:1px solid #333;border-radius:8px;background:#1c1c1c;color:#eee}\
         button{width:100%;margin-top:24px;padding:14px;font-size:16px;font-weight:600;\
         border:0;border-radius:8px;background:#2d7;color:#062}\
         </style></head><body><h1>Voicebox Setup</h1>\
         <p class=sub>Saving restarts the device.</p>\
         <form method=POST action=/save>",
    );
    for field in FIELD_ORDER {
        let value = current.get(field);
        html.push_str("<label for=");
        html.push_str(field.key());
        html.push('>');
        push_escaped(&mut html, field.label());
        html.push_str("</label><input id=");
        html.push_str(field.key());
        html.push_str(" name=");
        html.push_str(field.key());
        if field.is_secret() {
            // Never echo a stored secret back over an open access point. An
            // empty submission means "leave this one alone" (see `apply_form`),
            // so the placeholder has to say so.
            html.push_str(" type=password autocomplete=off placeholder=\"");
            push_escaped(&mut html, if value.is_empty() { field.hint() } else { "unchanged" });
            html.push('"');
        } else {
            // `type=url` on the base URLs is what makes the phone's own form
            // validation demand a scheme, so a bare host is rejected before it
            // is ever submitted rather than sending the device back to setup
            // on the next boot. Nothing is prefilled on a fresh device, so the
            // hint is the only thing telling the user what shape to use.
            html.push_str(if field.is_base_url() { " type=url inputmode=url" } else { " type=text" });
            html.push_str(" autocapitalize=off autocorrect=off placeholder=\"");
            push_escaped(&mut html, field.hint());
            html.push_str("\" value=\"");
            push_escaped(&mut html, value);
            html.push('"');
        }
        html.push('>');
    }
    html.push_str("<button type=submit>Save &amp; Restart</button></form></body></html>");
    html
}

fn saved_page() -> String {
    String::from(
        "<!DOCTYPE html><html><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>Saved</title><style>body{font-family:-apple-system,system-ui,sans-serif;\
         background:#111;color:#eee;margin:0;padding:40px 20px;text-align:center}\
         h1{font-size:22px}p{color:#888}</style></head><body>\
         <h1>Saved</h1><p>The device is restarting and will join your network. \
         This page will stop responding.</p></body></html>",
    )
}

fn push_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Folds a urlencoded form body onto a copy of the current settings.
///
/// A field that is absent, or present but empty *and* secret, keeps its old
/// value - that is what lets the form show blank password boxes without
/// wiping the stored credentials every time someone changes only the SSID.
fn apply_form(current: &Settings, body: &[u8]) -> Settings {
    let mut out = current.clone();
    for pair in body.split(|&b| b == b'&') {
        let Some(eq) = pair.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = urldecode(&pair[..eq]);
        let value = urldecode(&pair[eq + 1..]);
        let Some(field) = FIELD_ORDER.into_iter().find(|f| f.key() == key) else {
            continue;
        };
        if value.is_empty() && field.is_secret() {
            continue;
        }
        out.set(field, value);
    }
    out
}

fn urldecode(raw: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => {
                bytes.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < raw.len() => {
                match (hex(raw[i + 1]), hex(raw[i + 2])) {
                    (Some(h), Some(l)) => {
                        bytes.push(h << 4 | l);
                        i += 3;
                    }
                    // Not a valid escape - keep the '%' as a literal rather
                    // than silently dropping a character out of a password.
                    _ => {
                        bytes.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                bytes.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(bytes).unwrap_or_default()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

