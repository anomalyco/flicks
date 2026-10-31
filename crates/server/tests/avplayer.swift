// Test helper for `transcode_ladder_avplayer` in server.rs (compiled there with swiftc).
// Plays one HLS URL in AVPlayer from the start and again after a seek, and prints one
// `AVPLAYER {json}` line per run: timings, the decoded pixel-buffer size, and the item's
// access and error logs. URIs are printed with the token redacted. Exits 1 on failure.
import AVFoundation
import CoreMedia
import Foundation

final class Flag { var value = false }

func redact(_ s: String?) -> String {
    (s ?? "").replacingOccurrences(of: "token=[^&\\s\"]*", with: "token=REDACTED", options: .regularExpression)
}

func number(_ d: Double) -> Any { d.isFinite ? d : NSNull() }

func spin(_ timeout: Double, _ done: () -> Bool) -> Bool {
    let end = Date().addingTimeInterval(timeout)
    while Date() < end {
        if done() { return true }
        RunLoop.main.run(until: Date().addingTimeInterval(0.005))
    }
    return done()
}

let args = CommandLine.arguments
if args.count == 2 && args[1] == "--hdr-eligible" { print(AVPlayer.eligibleForHDRPlayback); exit(0) }
guard args.count == 3, let url = URL(string: args[1]), let seek = Double(args[2]) else {
    FileHandle.standardError.write("usage: avplayer <url> <seek-seconds>\n".data(using: .utf8)!)
    exit(2)
}

var ok = true
for target in [0.0, seek] {
    let item = AVPlayerItem(url: url)
    let output = AVPlayerItemVideoOutput(pixelBufferAttributes: nil)
    item.add(output)
    let player = AVPlayer(playerItem: item)
    player.isMuted = true
    let start = Date()
    var result: [String: Any] = ["phase": target == 0 ? "start" : "seek", "target": target]
    let ready = spin(30) { item.status != .unknown } && item.status == .readyToPlay
    result["ready_ms"] = Date().timeIntervalSince(start) * 1000
    var width = 0
    var height = 0
    var firstFrame: Double? = nil
    var position = 0.0
    if ready {
        if target > 0 {
            let sought = Flag()
            let seekStart = Date()
            player.seek(
                to: CMTime(seconds: target, preferredTimescale: 600), toleranceBefore: .zero, toleranceAfter: .zero
            ) { _ in sought.value = true }
            _ = spin(30) { sought.value }
            result["seek_ms"] = Date().timeIntervalSince(seekStart) * 1000
        }
        let playAt = Date()
        player.play()
        _ = spin(30) {
            let time = output.itemTime(forHostTime: CACurrentMediaTime())
            if output.hasNewPixelBuffer(forItemTime: time),
                let buffer = output.copyPixelBuffer(forItemTime: time, itemTimeForDisplay: nil)
            {
                width = CVPixelBufferGetWidth(buffer)
                height = CVPixelBufferGetHeight(buffer)
                if firstFrame == nil { firstFrame = Date().timeIntervalSince(playAt) * 1000 }
            }
            position = player.currentTime().seconds
            return item.status == .failed || (firstFrame != nil && position >= target + 1.5)
        }
        player.pause()
    }
    let passed = ready && item.status != .failed && firstFrame != nil && width > 0 && position >= target + 1.5
    ok = ok && passed
    result["ok"] = passed
    result["first_frame_ms"] = firstFrame.map(number) ?? NSNull()
    result["position"] = number(position)
    result["width"] = width
    result["height"] = height
    result["presentation"] = [number(Double(item.presentationSize.width)), number(Double(item.presentationSize.height))]
    result["error"] = redact(item.error.map { "\($0)" })
    result["access"] = (item.accessLog()?.events ?? []).map { e -> [String: Any] in
        [
            "uri": redact(e.uri), "requests": e.numberOfMediaRequests, "startup_s": number(e.startupTime),
            "stalls": e.numberOfStalls, "bytes": e.numberOfBytesTransferred,
            "transfer_s": number(e.transferDuration), "indicated_bps": number(e.indicatedBitrate),
            "observed_bps": number(e.observedBitrate), "watched_s": number(e.durationWatched),
            "dropped_frames": e.numberOfDroppedVideoFrames,
        ]
    }
    result["errors"] = (item.errorLog()?.events ?? []).map { e -> [String: Any] in
        ["status": e.errorStatusCode, "domain": e.errorDomain, "comment": redact(e.errorComment), "uri": redact(e.uri)]
    }
    let data = try! JSONSerialization.data(withJSONObject: result, options: [.sortedKeys])
    print("AVPLAYER " + String(decoding: data, as: UTF8.self))
    player.replaceCurrentItem(with: nil)
}
exit(ok ? 0 : 1)
