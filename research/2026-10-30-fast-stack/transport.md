# Fastest P2P transport for streaming large video between home media servers and clients (late 2026)

Scope note: runtime performance only (throughput, startup/seek latency, CPU per Gbit, NAT traversal). Baseline is the Deadbolt sync layer: `iroh = "1.3.0"` (`/Users/vogel/dev/deadbolt/crates/net/Cargo.toml`), endpoint built with `Endpoint::builder(presets::Minimal)`, n0 default relays or custom relay URLs, and **no custom `transport_config`**, so it runs on stock QUIC flow-control windows and the default congestion controller (`/Users/vogel/dev/deadbolt/crates/net/src/node.rs`, lines ~195–228). Data older than 2024 is marked **[older]**.

## 1. iroh / iroh-blobs real throughput (single vs multi-stream, BBR vs CUBIC, windows, CPU, GSO/GRO, LAN vs WAN)

### Takeaway
Stock iroh/iroh-blobs settings do not get close to line rate. The best public real-hardware numbers (2026) are about 40–50 MB/s single-stream on a 110 MB/s Mac LAN, and that needs BBR plus 32–64 MiB windows. CUBIC on the same path gave 1.3–1.7 MB/s. Part of the BBR shortfall was a noq bug, fixed in June 2026. Getting near line rate needs four things: tuned windows, BBR (or a fixed controller), several parallel range streams, and Linux GSO/GRO hosts. macOS hosts stay noticeably slower.

### Cited Findings
- iroh 1.x runs on **noq**, n0's hard fork of Quinn with QUIC Multipath and QUIC NAT traversal. It has been iroh's transport since v0.96. — [iroh blog: noq announcement](https://www.iroh.computer/blog/noq-announcement); [iroh docs: Using QUIC](https://docs.iroh.computer/protocols/using-quic)
- iroh 1.0 shipped mid-2026 (news on Jun 16 2026; n0 retrospective on Jul 9 2026). n0 describes blob transfer as "close to optimal performance" (vendor claim, no numbers given). — [TechTimes](https://www.techtimes.com/articles/318490/20260616/peer-peer-library-iroh-10-ships-dial-devices-key-not-ip-address.htm); [iroh blog: road to 1.0](https://www.iroh.computer/blog/the-road-to-iroh-1-0)
- **Issue #4286 (May 24 2026, iroh 0.98 / noq 0.18, two Apple-silicon Macs, Wi-Fi 6 ↔ 2.5 GbE, 1 GB random file):**
  - iperf3 UDP baseline on the same LAN: ~110 MB/s
  - iroh-blobs single stream with BBR: 42.65 MB/s
  - BBR plus tuned windows (`stream_receive_window` 32 MiB, `send_window` 64 MiB): 48.16 MB/s, about 45% of iperf3
  - Vanilla `QuicTransportConfig::builder().build()`: 37 MB/s
  - BBR with an in-memory store: 21.5 MB/s, slower than the filesystem store (unexplained)
  - CUBIC: 1.29 MB/s tuned and 1.70 MB/s vanilla, roughly 30× slower than BBR
  - Slow-case pattern: 32 MB bursts (one full stream window) at ~615 MB/s, then multi-second stalls
  - Source: [n0-computer/iroh#4286](https://github.com/n0-computer/iroh/issues/4286)
- Maintainer replies on #4286:
  - Arqu (n0): "GRO/GSO perf is really not that great on the mac platform. Linux is consistently much faster to run if you want to saturate link speed." He also asks whether the `fast-apple-datapath` feature was enabled.
  - matheus23 (n0) asks for a repro with iroh's `transfer` example, noting that CUBIC vs BBR should be independent of iroh-blobs.
  - The issue is still open, labeled `perf`, status "Ready". No fix is recorded on it.
  - Source: [iroh#4286](https://github.com/n0-computer/iroh/issues/4286)
- `initial_window` on the congestion controller is **not exposed** on `QuicTransportConfigBuilder` (reporter's open question 2). — [iroh#4286](https://github.com/n0-computer/iroh/issues/4286)
- **noq BBR3 bug:** BBR3 never received the per-packet `on_packet_sent` callback.
  - PR #657 (May 15 2026): "BBR controller bench now goes from 1.3 to 55 MiB/s on my setup and matches quinn and s2n."
  - The fix was re-done as noq #689, "Call on_packet_sent when ACK-eliciting data is sent", merged around Jun 3 2026.
  - noq's netsim CI reports ~782 Mbps under LAN conditions (as quoted in #4286).
  - Sources: [noq#657](https://github.com/n0-computer/noq/pull/657); [iroh#4286](https://github.com/n0-computer/iroh/issues/4286)
- The transport config has `stream_receive_window`, `receive_window` (per connection) and `send_window`. iroh docs say each should be at least latency × desired throughput (the BDP). — [docs.rs QuicTransportConfigBuilder](https://docs.rs/iroh/latest/iroh/endpoint/struct.QuicTransportConfigBuilder.html)
- **Third-party tuning writeup (kith, mid-2026):**
  - Windows: `stream_receive_window` 16 MiB, `receive_window` 64 MiB, `send_window` 64 MiB. This covers the ~6 MiB BDP of a 100 ms × 500 Mbit link and is called "the change that matters most on real, latent links."
  - Multi-stream fetch: N=4 concurrent BLAKE3-verified range GETs over separate QUIC streams on one connection.
  - Loopback, 64 MiB blob: single stream ~77 MB/s, 4 streams ~93 MB/s. Loopback is CPU-bound, so this shows the cost of bao verification and copying, not network limits.
  - BBR was *not* applied because iroh re-exports the `ControllerFactory` trait but no stable public BBR config; using it means pinning internal `noq_proto`.
  - Source: [kith docs/throughput.md](https://github.com/muhamadjawdatsalemalakoum/kith/blob/main/docs/throughput.md)
- **Quinn upstream (the base of noq), CPU profile [2023–2024 data]:** in a 10 Gbit/s testbed (Xeon W-2145, kernel 5.15, CUBIC, GSO on, 1500 MTU):
  - TCP reached 9.49 Gbit/s (iperf3) and 9.54 Gbit/s (netperf); raw UDP reached 9.74 Gbit/s.
  - No QUIC implementation saturated the link.
  - quinn "constantly uses primarily one core to 100%", i.e. it is single-core bound.
  - Disabling encryption gained msquic +22% and quinn only +7.3%.
  - Source: [König et al., KIT, LCN'23](https://publikationen.bibliothek.kit.edu/1000161904/152028985)
- ETH Zürich theses (2024–2025):
  - Bigger Quinn segments/windows raise throughput, "up to doubling it in some cases". A patched Quinn sending larger segments made fewer syscalls.
  - MsQuic was insensitive to window changes.
  - Quinn significantly outperformed quiche in their harness.
  - MsQuic beat Quinn by ≥50% under loss/reordering.
  - Sources: [Marti, ETH 2024](https://nsg.ee.ethz.ch/files/public/theses/2024-quic_benchmark_suite/thesis-1.pdf); [ETH qbench 2025](https://nsg.ee.ethz.ch/files/public/theses/2024-qbench/thesis-1.pdf); [TCP-over-QUIC tunnels, arXiv 2504.10054](https://arxiv.org/html/2504.10054v1)
- Quinn issue #2627 (cited by ant-quic): GSO bundles release up to 10 datagrams in ~12 µs, a ~5.8 Gbps wire spike. Pacing happens *between* `sendmsg` calls, not within a bundle, so CDN/CGNAT rate limiters can tail-drop bundles. — [ant-quic gso diagnostics](https://docs.rs/ant-quic/latest/ant_quic/diagnostics/gso/index.html)

### Inferences
- Deadbolt's current endpoint uses no transport config, so stock windows and default CC. Reused as-is for video it would land in #4286's "vanilla" row: tens of MB/s on a LAN and worse on WAN. Window tuning is mandatory, not optional.
- For video, the sustained rate that matters is modest. A 4K HDR remux is ~80–120 Mbit/s (10–15 MB/s), which even the untuned #4286 numbers exceed on a LAN. Raw throughput matters for **seek refill and prefetch bursts**, multi-client servers and WAN paths. On WAN, BDP-limited windows hurt most: a 64 KiB–1 MiB window at 50 ms RTT caps out at a few to ~20 MB/s.
- The noq BBR3 fix (#689, ~Jun 3 2026) probably predates iroh 1.0 (mid-June 2026), so iroh 1.3 likely has it. This is unverified: confirm by checking the noq version pinned by iroh 1.3. The #4286 CUBIC-30×-slower result was on pre-fix noq 0.18 and should be re-measured on 1.3.
- Recommended tuning to benchmark, in Rust/noq terms:
  1. `stream_receive_window` 16–32 MiB, `receive_window` 64 MiB, `send_window` 64 MiB.
  2. BBR via the noq `ControllerFactory`. This needs a pinned noq-proto version until iroh exposes it publicly.
  3. 2–4 parallel range streams per large blob.
  4. Linux servers, with `fast-apple-datapath` enabled on Mac builds.
  5. Measure with iroh's `transfer` example before involving iroh-blobs.
- A CPU-per-Gbit figure for iroh/noq was not published anywhere I found. Upstream Quinn being single-core-bound suggests a few Gbit/s per connection on desktop x86 as the practical ceiling. That is far above home-network needs, but relevant for low-power NAS CPUs (ARM, Celeron).

### Gaps
- No published iroh/noq numbers on Linux 10 GbE hardware, or WAN (RTT 20–100 ms) throughput curves for 1.x.
- No measured CPU% or cycles/byte for iroh-blobs (QUIC plus bao verification) on NAS-class hardware.
- Whether `initial_window` or a BBR config is publicly exposed in iroh 1.3 is unconfirmed. As of the June 2026 kith writeup, BBR was not.

## 2. Hole-punching success rates, relay fallback throughput, self-hosted iroh relay, n0 rate limits

### Takeaway
Vendor-reported direct-connection rates are about 90% for iroh and over 90% for Tailscale. The only large, independent, published measurement is libp2p DCUtR at 70% ± 7.1%. Relayed paths are much slower: Tailscale DERP measured 2.2 Mbit/s in one 2026 case, and n0 shared relays cap at 5 MB/s. For video, plan on a **self-hosted relay** (iroh-relay) or a home server acting as relay, with the per-client limit raised.

### Cited Findings
- **iroh:**
  - n0 says "roughly 9 out of 10 connections go direct" (FAQ).
  - Secondary sources quote ~95% of *data volume* going direct.
  - Source: [iroh FAQ](https://docs.iroh.computer/about/faq); secondary: [Pinggy blog, Jun 2026](https://pinggy.io/blog/iroh_1_0_dial_keys_not_ips), [CasualOffice RAS transport doc](https://github.com/CasualOffice/RASystem/blob/main/docs/09_TRANSPORT_IROH.md)
- iroh 1.0 does hole punching inside QUIC using the IETF QUIC-NAT-Traversal (QNT) draft, and treats relay/direct paths as QUIC Multipath paths. — [Pinggy](https://pinggy.io/blog/iroh_1_0_dial_keys_not_ips); [iroh on QUIC Multipath](https://www.iroh.computer/blog/iroh-on-QUIC-multipath)
- At startup an iroh endpoint sends QAD probes to every configured relay, picks the lowest-latency one as its "home relay", and holds a secure WebSocket to it. — [What is iroh?](https://docs.iroh.computer/what-is-iroh)
- The relay data path is HTTPS/WebSocket over TCP. A QUIC relay transport was requested in Jul 2026 (embedded memory reasons), and an old `relay-h3` (HTTP/3) branch exists but is not merged. — [iroh#4395](https://github.com/n0-computer/iroh/issues/4395)
- **n0 public relays:**
  - Four regions: US West, US East, Europe Central, Singapore.
  - Rate-limited based on total traffic across all users; no SLA; "development and testing" only.
  - No numeric limit is published.
  - Sources: [iroh FAQ](https://docs.iroh.computer/about/faq); [Public Relays](https://docs.iroh.computer/iroh-services/relays/public); [Shared Relays](https://docs.iroh.computer/iroh-services/relays/shared)
- **n0 Shared Relays (Pro):**
  - $19/month for four global relays and up to 10,000 concurrent connections, then $0.003 per endpoint per month.
  - 100 GB relay egress per month, then $0.09/GB.
  - **5 MB/s rate limit.** Only relayed traffic counts.
  - Dedicated relays have "no rate limits at all".
  - Sources: [Shared Relays](https://docs.iroh.computer/iroh-services/relays/shared); [Rate Limiting](https://docs.iroh.computer/relays/rate-limiting)
- **Self-hosted iroh-relay:**
  - Docker image `n0computer/iroh-relay`; a TOML config with ACME/Let's Encrypt on :80/:443 is the minimum.
  - Per-client limits use a token bucket: `[limits.client.rx] bytes_per_second`, `max_burst_bytes`, or `iroh_relay::server::Limits::client_rx` when embedded.
  - The `accept_conn_limit` fields exist but are not enforced yet.
  - Configure clients with `RelayMode::Custom`.
  - Sources: [Self-hosted](https://docs.iroh.computer/iroh-services/relays/self-hosted); [Rate Limiting](https://docs.iroh.computer/relays/rate-limiting)
- **Tailscale:**
  - "Internal metrics have indicated success rates for direct NAT traversal well north of 90% in typical conditions" (Oct 2025).
  - DERP "limit[s] throughput to ensure fairness".
  - Sources: [Tailscale NAT traversal improvements pt 1](https://tailscale.com/blog/nat-traversal-improvements-pt-1); [Poor performance docs](https://tailscale.com/docs/reference/troubleshooting/poor-performance-tailnet)
- **Tailscale relay numbers (Jan 2026), Delhi → Minneapolis, CGNAT/symmetric NAT on Indian ISPs:**
  - The ISP connection itself tests at 30–40 Mbit/s internationally.
  - DERP: 2.2 Mbit/s sustained over 120 s, with frequent 0-bit/s seconds.
  - Peer Relay (a home node with a forwarded UDP port): 27.5 Mbit/s, about 12.5× DERP.
  - RTT: 441–478 ms via DERP vs 298–306 ms via peer relay.
  - Source: [Tailscale blog: peer relays international](https://tailscale.com/blog/peer-relays-international-networks)
- Tailscale Peer Relays (beta Oct 2025, now GA): "throughputs nearing that of a direct connection; often multiple orders of magnitude higher than Tailscale's managed DERP fleet." — [Peer Relays beta](https://tailscale.com/blog/peer-relays-beta); [Peer Relays GA](https://tailscale.com/blog/peer-relays-ga)
- **libp2p DCUtR (the largest independent study):**
  - Scale: 4.4M traversal attempts, 85k+ networks, 167 countries.
  - **70% ± 7.1%** hole-punch success, *conditional* on relay reservation and public-address discovery succeeding first. About 21% of data points had no public address advertised at all.
  - TCP and QUIC succeed at statistically the same rate (~70%).
  - 97.6% of successes happen on the first attempt.
  - After a punch, 50% of peers had ≤70% of their relayed RTT, and 10% got a *higher* RTT.
  - Sampling bias may inflate the rate: volunteer clients, and remote peers already reachable via relay.
  - Data was collected Dec 2022–Jan 2023 but published Oct 2025 / Apr 2026.
  - Sources: [arXiv 2510.27500](https://arxiv.org/html/2510.27500v1); [arXiv 2604.12484](https://arxiv.org/pdf/2604.12484)
- WebRTC ICE: the same paper describes it as STUN → hole punch → TURN, with TURN carrying the full stream. iroh's FAQ calls TURN servers "stateful and expensive to run at scale". — [arXiv 2510.27500](https://arxiv.org/html/2510.27500v1); [TechTimes summarizing iroh FAQ](https://www.techtimes.com/articles/318490/20260616/peer-peer-library-iroh-10-ships-dial-devices-key-not-ip-address.htm)

### Inferences
- Expect about 10% of remote client↔server pairs (more on CGNAT mobile networks and in India/Asia) to be relay-only. On n0 shared relays that means ≤5 MB/s (40 Mbit/s): fine for 1080p and borderline for 4K remux. On public relays it is unknown but throttled, so unusable for production video.
- Every home server is a natural relay candidate, because many have a forwardable port or UPnP. Running `iroh-relay` (embedded or the Docker image) on one or more home servers, or on a cheap VPS, mirrors Tailscale Peer Relays and removes per-client caps. Cost is mostly egress bandwidth. Relay CPU is TLS over TCP plus forwarding, so it is light.
- iroh relays carry traffic over TCP/WebSocket, so relayed QUIC runs over TCP. That adds head-of-line blocking and extra latency compared with a UDP relay like Tailscale Peer Relays or TURN-UDP. A future QUIC/H3 relay (#4395) would help.

### Gaps
- No numeric limit is published for n0 public relays.
- No independent measurement of iroh's direct rate. The 90%/95% figures are vendor-reported.
- No published WebRTC ICE / TURN-fallback percentages from 2024–2026 were found in this pass.
- No measured throughput or CPU for a self-hosted iroh-relay forwarding one high-bitrate stream.

## 3. Alternatives: plain QUIC (quinn, s2n-quic, quiche, msquic), Tailscale/tsnet/WireGuard, libp2p, TCP+TLS/HTTP2/HTTP3; kernel vs userspace

### Takeaway
On raw throughput per CPU, kernel TCP+TLS (kTLS) still beats every userspace QUIC stack. The gap is about 2× at 10 Gbit/s and up to 45% fewer bytes/s on 1 Gbit/s clients. The best userspace QUIC stacks reach roughly 4–7 Gbit/s per connection with GSO/GRO on Linux. That is far above home-video needs, so the choice turns on NAT traversal and connection identity, where iroh/QUIC wins. In-kernel QUIC exists only as 2025 patches and is currently slower than kTLS.

### Cited Findings
- **TCP vs QUIC at 10 Gbit/s [2023–24]:** TCP 9.49–9.54 Gbit/s vs QUIC implementations well below line rate. The QUIC gap is "more than overhead by cryptography" (16.1% to 297.5% spread across implementations). lsquic ~4.15 Gbit/s pinned to one core, dropping to 3.7 Gbit/s when rescheduled. — [König et al., KIT LCN'23](https://publikationen.bibliothek.kit.edu/1000161904/152028985); [KIT BelWü TechDay'24 slides](https://publikationen.bibliothek.kit.edu/1000169833/152613999)
- **GSO/GRO effect (IFIP Networking 2025):** quiche went from 3.5 to 4.16 Gbit/s and ngtcp2 from 3.7 to 4.94 Gbit/s with GSO/GRO. picoquic with DPDK kernel bypass reached up to 20 Gbit/s, nearly 3× its usual figure. — [König et al., IFIP 2025](http://doc.tm.kit.edu/2025-Examining-the-Heterogeneous-Throughput-Performance-Landscape-of-QUIC-Implementations-Koenig-et-al.pdf)
- [older, 2023] QUIC on newer Intel nearly reached 5 Gbit/s, and GSO/GRO hardly changed LSQUIC/quiche goodput there, while TCP benefited most from TSO. — [Jaeger et al., QUIC on the Highway](https://arxiv.org/pdf/2309.16395)
- **"QUIC is not Quick Enough over Fast Internet" (WWW'24):**
  - UDP+QUIC+HTTP/3 delivers up to 45.2% less data rate than TCP+TLS+HTTP/2, and the gap grows with bandwidth.
  - QUIC becomes the bottleneck above ~600 Mbps on lightweight clients.
  - Video streaming lost up to 9.8% bitrate.
  - Chrome QUIC downloads fell to ~200 Mbps at low CPU frequency.
  - Root cause: receiver-side overhead from too many packets and userspace ACKs.
  - Source: [Zhang et al., WWW'24](https://dl.acm.org/doi/abs/10.1145/3589334.3645323); [arXiv PDF](https://arxiv.org/pdf/2310.09423)
- **Tailscale data plane (wireguard-go, Linux) [2023]:**
  - With UDP GSO/GRO and checksum unrolling: 13.0 Gbit/s single-stream TCP on i5-12400 with 25G NICs, vs 11.8 Gbit/s for in-kernel WireGuard.
  - 7.3 Gbit/s on AWS c6i.8xlarge, an underlay cap.
  - Older E3-1230v2 went from 3.36 to 6.59 Gbit/s with hardware UDP segmentation.
  - Source: [Surpassing 10Gb/s over Tailscale](https://tailscale.com/blog/more-throughput)
- Tailscale on QUIC/UDP throughput [2023]: msquic secnetperf reached 7.2 Gbit/s over wireguard-go on c6i.8xlarge after the UDP offload work, which Tailscale calls a ">4x" UDP gain. This needs Tailscale ≥1.54 and Linux ≥6.2, plus the `rx-udp-gro-forwarding` ethtool setting for forwarders. — [Tailscale QUIC/UDP throughput](https://tailscale.com/blog/quic-udp-throughput); [Tailscale performance best practices](https://tailscale.com/docs/reference/best-practices/performance.md)
- Tailscale Sep 2026: multi-queue throughput for exit nodes and subnet routers (H2 2026), `writev` gains, and netmap caching to cut ~100 ms of startup latency. — [We're making Tailscale faster](https://tailscale.com/blog/making-tailscale-faster)
- **In-kernel QUIC (Linux):**
  - Xin Long's `IPPROTO_QUIC` patchsets: v1 Jul 2025, v2 Aug 2025, v3 Sep 2025. The handshake stays in userspace through tlshd.
  - In the posted benchmarks, kTLS had nearly 3× in-kernel QUIC's throughput, and TCP beat unencrypted QUIC by more than 4×.
  - Reasons given: no GSO, an extra copy, and header encryption.
  - Mainline was expected "sometime in 2026 at best".
  - Sources: [LWN: QUIC for the kernel](https://lwn.net/Articles/1029851); [LWN patch v2](https://lwn.net/Articles/1034269); [draft-lxin-quic-socket-apis](https://www.ietf.org/archive/id/draft-lxin-quic-socket-apis-02.html)
- Congestion-control behavior "of the same congestion control schemes vary significantly by implementation". Authors urge naming algorithm, implementation and version (USENIX ATC'25). — [Yuan et al., ATC'25](https://www.usenix.org/system/files/atc25-yuan.pdf)
- libp2p: DCUtR hole punching works equally well over TCP and QUIC (~70%). Circuit v2 relays are deliberately limited in duration and data, so they are signaling-only and cannot carry bulk traffic. — [arXiv 2510.27500](https://arxiv.org/html/2510.27500v1)

### Inferences
- Ranking for a home-media byte-range workload, runtime only:
  1. **Kernel TCP+TLS / HTTP/2 on LAN or with a port forward**: best bytes per CPU and simplest range requests, but no NAT traversal.
  2. **iroh/noq QUIC, tuned**: NAT traversal, key-addressed identity and multiplexed range streams. Per-connection ceiling is likely a few Gbit/s on Linux, but stock tuning and macOS datapath are the current weak spots.
  3. **Tailscale/WireGuard (kernel mode or tsnet)**: proven 10+ Gbit/s data plane and the best-documented NAT traversal and relay story. It requires a Tailscale or Headscale control plane, which conflicts with "no central account server" unless Headscale runs self-hosted.
  4. **libp2p**: comparable NAT traversal (~70% measured) but no relay suitable for bulk, plus heavier stacks.
- For 1 Gbit/s home LANs and ≤1 Gbit/s WAN, any well-tuned option saturates the link. The differentiators are CPU on low-power NAS/clients (TCP/kTLS is cheapest), NAT traversal, and relay quality.
- A hybrid is reasonable: use iroh for discovery and control and for remote/NAT'd data, and when both peers share a LAN or a server has a reachable port, optionally serve ranges over HTTP/1.1 or HTTP/2 with TLS bound to the iroh key. That gives kernel-TCP efficiency on the hottest path. Benchmark before adopting.

### Gaps
- No 2024–2026 head-to-head of s2n-quic vs quinn vs msquic vs quic-go on identical hardware with GSO/GRO was found. The KIT studies cover subsets.
- No tsnet (gVisor netstack) throughput numbers were found. tsnet runs userspace TCP over wireguard-go and is likely slower than kernel-mode Tailscale, but this is unverified.
- Whether the in-kernel QUIC patches merged into mainline in 2026 was not confirmed.

## 4. Browser reach: WebTransport, WebRTC data channels, iroh in wasm (relay-only)

### Takeaway
iroh in the browser is **relay-only**, so browser playback over iroh is capped by relay throughput: 5 MB/s on n0 shared relays, or whatever a self-hosted relay allows. Direct browser paths need WebTransport (HTTP/3, with `serverCertificateHashes` for self-signed home servers) or WebRTC data channels. No rigorous 2024–2026 bulk-throughput benchmark of either was found. Old data showed WebRTC SCTP throughput collapsing as RTT grows.

### Cited Findings
- iroh compiles to wasm (since 0.33), but "All connections from browsers to somewhere else need to flow via a relay server", because browsers cannot send UDP to arbitrary IPs. The connection is still end-to-end encrypted. n0 lists WebTransport with `serverCertificateHashes` or WebRTC as possible future direct paths. — [iroh docs: WebAssembly and Browsers](https://docs.iroh.computer/languages/wasm-browser); [deployment page](https://docs.iroh.computer/deployment/wasm-browser-support)
- Browser transport micro-benchmark (loopback, Go echo server, 512 B messages):
  - RTT p50: WebSocket 0.050–0.055 ms, WebTransport stream/datagram 0.060 ms, WebRTC data channel 0.085–0.090 ms, HTTP/2 fetch 0.240 ms.
  - Throughput was message-bound at ~46–53 Mbit/s for all modern transports with 64 messages in flight. This is not representative of bulk transfer.
  - Source: [yohimik/ws-webrtc-benchmark](https://github.com/yohimik/ws-webrtc-benchmark)
- PeerProxy thesis (May 2025): browser TCP streaming reached up to 1400 MB/s at 20 ms RTT. WebRTC data-channel throughput stayed below a theoretical maximum that falls with RTT (an SCTP window limit). — [Lee, PeerProxy thesis](https://nathanlee.info/thesis.pdf)
- [older, 2015] Browser WebRTC implementations did not adjust the SCTP window from its default, so throughput dropped at high RTT. — [Eskola & Nurminen](https://tuhat.helsinki.fi/ws/portalfiles/portal/167373638/Eskola_webrtc.pdf)
- A WebRTC issue-tracker report cited ~140 Mbps as the "known" local data-channel max (date unclear). Chrome has moved from usrsctp to dcSCTP. — [search result: issues.webrtc.org](https://issues.webrtc.org); [dcSCTP note](https://www.fanyamin.com)
- Browser QUIC is CPU-sensitive: Chrome QUIC fell to ~200 Mbps at reduced CPU frequency, against higher HTTP/2 throughput on the same machine. — [arXiv 2310.09423](https://arxiv.org/pdf/2310.09423)
- MoQ (Media over QUIC) uses WebTransport for browser reach, "trad[ing] some performance for reach". A moq-rs relay is a few thousand lines of Rust on quiche. — [Fora Soft MoQ 2026](https://www.forasoft.com/learn/video-streaming/articles-streaming/media-over-quic-moq)

### Inferences
- The best-performing browser path is **plain HTTPS range requests (HTTP/2 or HTTP/3) to a home server**, through a port forward or a reverse tunnel/relay. Browsers' native `<video>` and MSE pipelines already handle range-based seeking, and TCP/H2 in browsers is the most CPU-efficient option.
- WebTransport with `serverCertificateHashes` gives direct QUIC to a home server with a self-signed cert, but the server still needs a reachable UDP port: a browser cannot hole-punch. A small iroh-to-WebTransport/HTTP gateway on each home server is a likely design.
- WebRTC data channels are the only browser option that hole-punches. Their bulk throughput is SCTP-limited (historically ~100–150 Mbit/s locally and much lower at high RTT), which is still enough for most video bitrates.

### Gaps
- No 2024–2026 measurement of WebTransport bulk download throughput (MB/s vs RTT) in Chrome, Firefox or Safari was found. Safari WebTransport support status was not verified.
- No measurement of iroh-wasm relay throughput in a browser.

## 5. Multi-source range fetching and BLAKE3/bao verified-streaming overhead

### Takeaway
iroh-blobs already speaks BLAKE3-verified range requests in 16 KiB chunk groups. A bad provider is detected within ≤16 KiB, which makes BitTorrent-style multi-source fetching safe. Verification costs CPU, not bandwidth: the outboard is about 0.4% at 16 KiB groups. On loopback, bao/BLAKE3 verification is the bottleneck, and parallelizing ranges across streams helped about 20%. Stream scheduling should follow libtorrent's deadline-aware "time-critical piece" picker, not rarest-first.

### Cited Findings
- iroh-blobs:
  - Content is addressed by the BLAKE3 root hash.
  - Range requests stream only the needed tree nodes.
  - Chunk hashes are cached as an external "outboard", leaving the original file unaltered as the source of bytes.
  - Sources: [iroh docs: Blobs](https://docs.iroh.computer/protocols/blobs); [iroh blog: Blob store design](https://www.iroh.computer/blog/blob-store-design-challenges)
- "The requester will notice if data is incorrect after at most 16 KiB of data." — [iroh blog: Blob store design challenges](https://www.iroh.computer/blog/blob-store-design-challenges)
- iroh-blobs chunk group size is 16 KiB. A resumed fetch restarts at the next unverified 16 KiB boundary. FsStore builds and persists the outboard on import, so serving does not recompute it. — [decdn ADR-038](https://github.com/decdn/decdn/blob/main/adr/038-bao-verified-range-streaming.md)
- The iroh-blobs downloader takes a list of providers (`downloader.download(hash_and_format, [provider_ids])`). — [iroh#4286 repro code](https://github.com/n0-computer/iroh/issues/4286)
- Multi-stream range fetch (4 streams, one connection): 77 → 93 MB/s on loopback because it "parallelizes the CPU-bound bao/BLAKE3" work. — [kith throughput.md](https://github.com/muhamadjawdatsalemalakoum/kith/blob/main/docs/throughput.md)
- BLAKE3 speed: SIMD (SSE2/SSE4.1/AVX2/AVX-512/NEON/Wasm) plus optional multithreading. The paper reports throughput scaling past ~140 GiB/s on 48 threads. Single-thread throughput in cycles/byte is shown in its Figure 3 (AWS c5.metal, AVX-512). — [BLAKE3 spec](https://raw.githubusercontent.com/BLAKE3-team/BLAKE3-specs/master/blake3.pdf); [BLAKE3 hazmat API blog](https://www.iroh.computer/blog/blake3-hazmat-api)
- The original motivation for Bao was streaming a video attachment while verifying each byte, and allowing random seeks. — [oconnor663/bao](https://github.com/oconnor663/bao)
- libtorrent streaming: for time-critical pieces it keeps peers sorted by *estimated download queue time* (outstanding bytes ÷ estimated rate) and requests from the peer that will deliver soonest. It notes the need to "support streaming from many slow peers". Sequential mode exists but is not the same thing. — [libtorrent streaming](https://www.libtorrent.org/streaming.html); [libtorrent manual](https://www.libtorrent.org/manual-ref.html)

### Inferences
- **Outboard overhead (my calculation, not from a source):** a binary tree over 16 KiB leaves has ~1 parent node (64 B) per leaf, so ~64 B / 16 KiB ≈ 0.4% extra on disk and on the wire for full-file transfers. Small ranges also carry O(log n) sibling hashes: 2^n leaves → ~n × 32 B, i.e. under 1 KiB of proof per range request even for 100 GB files. Plain bao with 1 KiB chunks would be ~6%, which is why iroh uses chunk groups.
- Seek alignment: a range request should be rounded to 16 KiB boundaries. The first verified bytes arrive after one 16 KiB group plus its proof, which is negligible.
- Multi-source plan:
  1. One QUIC connection per provider.
  2. Assign the next few seconds of playback (the "time-critical" window) to the provider with the lowest estimated queue time, as libtorrent does.
  3. Fill the prefetch window with larger ranges spread across providers.
  4. Use hedged requests (send the same urgent range to two providers and cancel the loser) only for the first segment after a seek.
- A home server with AVX2/NEON will hash or verify far faster than its network. BLAKE3 matters only on very weak ARM SBCs or when verifying many concurrent streams.

### Gaps
- Whether iroh-blobs' downloader *splits* one blob across multiple providers, rather than using failover only, was not confirmed.
- No published per-core verify throughput for iroh-blobs' bao implementation on NAS-class CPUs.

## 6. Seek latency: time from range request to first byte

### Takeaway
On an already-open connection, seek latency is about 1 RTT plus server disk read plus one 16 KiB verify group, for iroh streams, HTTP/2 and HTTP/3 alike. Opening a QUIC stream needs no handshake. The large costs are connection setup (QUIC 1-RTT = 2 RTT to first byte, TCP+TLS 1.3 = 3 RTT) and **relay-first startup**: iroh connections begin on the relay and upgrade to direct, so the first seconds may run at relay speed and RTT. Keep connections warm and pre-dial.

### Cited Findings
- Illustrative RTT arithmetic at 80 ms RTT (author's arithmetic, not measurements):
  - TCP + TLS 1.3, new connection: 3 RTT (240 ms)
  - QUIC 1-RTT, new connection: 2 RTT (160 ms)
  - QUIC 0-RTT, resumed: 1 RTT (80 ms)
  - TLS 1.3 early data: 2 RTT
  - The real saving vs modern TCP is one RTT.
  - Source: [QUIC/HTTP3 deep dive, Oct 2026](https://iotdigitaltwinplm.com/quic-http3-protocol-deep-dive-handshake-streams-loss-recovery-congestion-control-2026)
- [older, 2017] At Google scale, QUIC handshake latency is measured as 0 ms for 0-RTT, and the savings grow with min-RTT. — [Langley et al., SIGCOMM'17](https://people.cs.rutgers.edu/~sn624/552-F19/papers/quic.pdf)
- [older, 2021] Overall QUIC performance "is largely determined by the server's choice of congestion-control algorithm" more than by handshake savings. — [Yu & Benson, WWW'21](https://cs.brown.edu/people/tab/papers/QUIC_WWW21.pdf)
- Tailscale connections start relayed through DERP and are then upgraded. Tailscale is cutting ~100 ms of startup latency with netmap caching (2026). — [Tailscale connection types](https://tailscale.com/docs/reference/connection-types); [Making Tailscale faster](https://tailscale.com/blog/making-tailscale-faster)
- iroh connections first exist on the relay path; endpoints then exchange details and upgrade to direct. In 1.x both paths are QUIC Multipath paths. Fork-era measurements of connection time included both relay and direct-path setup. — [Why we forked Quinn](https://www.iroh.computer/blog/why-we-forked-quinn); [iroh on QUIC Multipath](https://www.iroh.computer/blog/iroh-on-QUIC-multipath)
- After a successful DCUtR punch, the median peer's RTT dropped to ≤70% of the relayed RTT. About 10% got worse. — [arXiv 2510.27500](https://arxiv.org/html/2510.27500v1)
- WebTransport browser RTT on loopback is ~0.06 ms p50, WebRTC ~0.09 ms. Transport overhead is negligible next to network RTT. — [ws-webrtc-benchmark](https://github.com/yohimik/ws-webrtc-benchmark)
- The #4286 slow case showed multi-second stalls after each 32 MB window. Untuned flow control can turn a seek-refill burst into seconds of stall. — [iroh#4286](https://github.com/n0-computer/iroh/issues/4286)

### Inferences
- Target seek latency for remote playback on a warm iroh direct connection at 30 ms RTT: about 30 ms network + 1–10 ms SSD (or 10–20 ms HDD seek) + under 1 ms verify, so roughly 40–60 ms to first verified byte. Over a relay it is about 1.5–3× the RTT plus relay queueing.
- Design levers:
  1. Keep one long-lived iroh connection per server per client.
  2. Pre-dial all known servers at app start so hole punching finishes before playback.
  3. Make the first range after a seek small (e.g. 256 KiB–1 MiB) to start decoding fast, then widen to multi-MiB ranges across parallel streams.
  4. Raise windows so the post-seek burst is not flow-control-stalled.
  5. Index keyframe offsets server-side so seeks request exact byte ranges.
- QUIC 0-RTT matters only for cold connections. iroh's support for 0-RTT/0.5-RTT is mentioned in docs ("Server-side 0.5-RTT"), but resumption behavior was not verified here.

### Gaps
- No measured time-to-first-byte for iroh (relay vs direct), Tailscale or WebTransport in a video-seek scenario.
- No published measurement of iroh's relay→direct upgrade time distribution.
