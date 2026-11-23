#!/usr/bin/env python3
"""Checks an MPEG-TS file from the iOS app against docs/specs/ios-app.md, section 11 (OL-64d).

    ios/check-stream.py <file.ts> [--width 1280 --height 720 --fps 30 --gop 30
                                   --sample-rate 48000 --channels 2] [--ffprobe ffprobe]

From the bytes: 188-byte packets, continuity counters, one program, H.264 as stream type 0x1B on
PID 0x0100 (the PCR PID) and AAC as 0x0F on PID 0x0101, a PCR at least every 40 ms and PAT/PMT
at least every 100 ms. From ffprobe: H.264 High at the size and rate, AAC-LC at the rate and
channel count, a keyframe every --gop frames, no B-frames. It also measures the peak-to-average
bitrate (docs/specs/ios-app.md, section 16): the largest one-second window of video bytes over the
average, and the same for the whole transport stream. Prints a report (appended to
$GITHUB_STEP_SUMMARY when set) and exits 1 when a check fails.
"""

import argparse
import json
import os
import subprocess
import sys

PACKET = 188
VIDEO_PID = 0x0100
AUDIO_PID = 0x0101
PMT_PID = 0x1000
PCR_MAX = 40 * 90  # 90 kHz ticks
PSI_MAX = 100 * 90


def parse_ts(data):
    """Facts about the transport stream itself."""
    out = {
        "size_ok": len(data) % PACKET == 0 and len(data) > 0,
        "sync_ok": True,
        "cc_errors": 0,
        "programs": [],
        "pmt": None,
        "pcrs": [],  # (packet index, pcr)
        "pats": [],  # packet index
        "pcr_pids": set(),
    }
    last_cc = {}
    for n in range(len(data) // PACKET):
        p = data[n * PACKET:(n + 1) * PACKET]
        if p[0] != 0x47:
            out["sync_ok"] = False
            continue
        pusi = bool(p[1] & 0x40)
        pid = (p[1] & 0x1F) << 8 | p[2]
        afc = (p[3] >> 4) & 0x03
        cc = p[3] & 0x0F
        if pid != 0x1FFF:
            if pid in last_cc:
                want = (last_cc[pid] + 1) & 0x0F if afc & 1 else last_cc[pid]
                if cc != want:
                    out["cc_errors"] += 1
            last_cc[pid] = cc
        offset = 4
        if afc & 2:
            length = p[4]
            if length > 0 and p[5] & 0x10:
                b = p[6:12]
                base = b[0] << 25 | b[1] << 17 | b[2] << 9 | b[3] << 1 | b[4] >> 7
                out["pcrs"].append((n, base))
                out["pcr_pids"].add(pid)
            offset = 5 + length
        if not afc & 1 or not pusi:
            continue
        payload = p[offset:]
        if pid in (0, PMT_PID) and payload:
            section = payload[1 + payload[0]:]
            length = (section[1] & 0x0F) << 8 | section[2]
            section = section[:3 + length]
            if pid == 0:
                out["pats"].append(n)
                out["programs"] = [
                    (section[i] << 8 | section[i + 1], (section[i + 2] & 0x1F) << 8 | section[i + 3])
                    for i in range(8, len(section) - 4, 4)
                ]
            elif out["pmt"] is None:
                pcr_pid = (section[8] & 0x1F) << 8 | section[9]
                info = (section[10] & 0x0F) << 8 | section[11]
                i = 12 + info
                streams = []
                while i + 5 <= len(section) - 4:
                    streams.append((section[i], (section[i + 1] & 0x1F) << 8 | section[i + 2]))
                    i += 5 + ((section[i + 3] & 0x0F) << 8 | section[i + 4])
                out["pmt"] = {"pcr_pid": pcr_pid, "streams": streams}
    return out


def gaps(values):
    return [b - a for a, b in zip(values, values[1:])]


def psi_gaps(ts):
    """Stream time between PATs, from the last PCR before each."""
    pcrs = ts["pcrs"]
    times, j, current = [], 0, None
    for n in ts["pats"]:
        while j < len(pcrs) and pcrs[j][0] < n:
            current = pcrs[j][1]
            j += 1
        if current is not None:
            times.append(current)
    return gaps(times)


def ffprobe(binary, path, *args):
    cmd = [binary, "-v", "error", "-of", "json", *args, path]
    return json.loads(subprocess.run(cmd, check=True, capture_output=True, text=True).stdout)


def peak_to_average(times, sizes, window=1.0):
    """The largest `window`-second sum of sizes over the average per window."""
    if len(times) < 2:
        return None, None, None
    duration = times[-1] - times[0]
    if duration <= 0:
        return None, None, None
    average = sum(sizes) * 8 / duration
    peak, total, j = 0, 0, 0
    for i in range(len(times)):
        total += sizes[i]
        while times[i] - times[j] >= window:
            total -= sizes[j]
            j += 1
        if times[i] - times[0] >= window - 1e-6 or i == len(times) - 1:
            peak = max(peak, total * 8 / window)
    return average, peak, peak / average


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("file")
    ap.add_argument("--width", type=int, default=1280)
    ap.add_argument("--height", type=int, default=720)
    ap.add_argument("--fps", type=int, default=30)
    ap.add_argument("--gop", type=int, default=30)
    ap.add_argument("--sample-rate", type=int, default=48000)
    ap.add_argument("--channels", type=int, default=2)
    ap.add_argument("--ffprobe", default="ffprobe")
    ap.add_argument("--no-ffprobe", action="store_true", help="only the transport stream checks")
    a = ap.parse_args()

    with open(a.file, "rb") as f:
        data = f.read()
    results = []  # (ok, what, detail)

    def check(ok, what, detail=""):
        results.append((bool(ok), what, detail))

    ts = parse_ts(data)
    check(ts["size_ok"] and ts["sync_ok"], "188-byte packets with sync bytes", f"{len(data)} bytes")
    check(ts["cc_errors"] == 0, "continuity counters", f"{ts['cc_errors']} errors")
    check(len(ts["programs"]) == 1 and ts["programs"][0][1] == PMT_PID, "one program, PMT on 0x1000",
          ", ".join(f"program {n} PMT 0x{p:04x}" for n, p in ts["programs"]))
    pmt = ts["pmt"] or {"pcr_pid": None, "streams": []}
    check(pmt["streams"] == [(0x1B, VIDEO_PID), (0x0F, AUDIO_PID)],
          "H.264 as 0x1B on 0x0100, AAC as 0x0F on 0x0101",
          ", ".join(f"type 0x{t:02x} PID 0x{p:04x}" for t, p in pmt["streams"]))
    check(pmt["pcr_pid"] == VIDEO_PID and ts["pcr_pids"] == {VIDEO_PID}, "PCR on the video PID",
          f"PMT PCR_PID {pmt['pcr_pid']}, PCRs seen on {sorted(ts['pcr_pids'])}")
    pcr_gap = max(gaps([v for _, v in ts["pcrs"]]) or [0])
    check(0 < pcr_gap <= PCR_MAX, "PCR at least every 40 ms", f"largest gap {pcr_gap / 90:.1f} ms")
    psi_gap = max(psi_gaps(ts) or [0])
    check(0 < psi_gap <= PSI_MAX, "PAT/PMT at least every 100 ms", f"largest gap {psi_gap / 90:.1f} ms")

    rates = {}
    if not a.no_ffprobe:
        info = ffprobe(a.ffprobe, a.file, "-show_programs", "-show_streams")
        check(len(info.get("programs", [])) == 1, "ffprobe: one program", f"{len(info.get('programs', []))}")
        streams = info.get("streams", [])
        v = next((s for s in streams if s.get("codec_type") == "video"), {})
        au = next((s for s in streams if s.get("codec_type") == "audio"), {})
        fps = v.get("r_frame_rate", "")
        check(v.get("codec_name") == "h264" and v.get("profile") == "High", "ffprobe: H.264 High",
              f"{v.get('codec_name')} {v.get('profile')} level {v.get('level')}")
        check((v.get("width"), v.get("height")) == (a.width, a.height), f"ffprobe: {a.width}x{a.height}",
              f"{v.get('width')}x{v.get('height')}")
        check(fps in (f"{a.fps}/1", f"{a.fps * 1000}/1000"), f"ffprobe: {a.fps} fps", fps)
        check(v.get("id") == f"0x{VIDEO_PID:x}", "ffprobe: video on 0x100",
              f"id {v.get('id')}, codec_tag {v.get('codec_tag')}")
        check(v.get("has_b_frames", 0) == 0, "ffprobe: has_b_frames 0", f"{v.get('has_b_frames')}")
        check(au.get("codec_name") == "aac" and au.get("profile") == "LC", "ffprobe: AAC-LC",
              f"{au.get('codec_name')} {au.get('profile')}")
        check(au.get("sample_rate") == str(a.sample_rate) and au.get("channels") == a.channels,
              f"ffprobe: {a.sample_rate} Hz, {a.channels} channels",
              f"{au.get('sample_rate')} Hz {au.get('channels')} ch {au.get('channel_layout')}")
        check(au.get("id") == f"0x{AUDIO_PID:x}", "ffprobe: audio on 0x101",
              f"id {au.get('id')}, codec_tag {au.get('codec_tag')}")

        frames = ffprobe(a.ffprobe, a.file, "-select_streams", "v:0", "-show_frames",
                         "-show_entries", "frame=pict_type,key_frame").get("frames", [])
        keys = [i for i, f in enumerate(frames) if f.get("key_frame") == 1]
        types = sorted({f.get("pict_type") for f in frames})
        want = list(range(0, len(frames), a.gop))
        check(len(frames) > 0 and keys == want, f"a keyframe every {a.gop} frames",
              f"{len(frames)} frames, keyframes at {keys[:12]}{' ...' if len(keys) > 12 else ''}")
        check("B" not in types, "no B-frames", f"picture types {types}")

        packets = ffprobe(a.ffprobe, a.file, "-select_streams", "v:0", "-show_entries",
                          "packet=dts_time,size").get("packets", [])
        times = [float(p["dts_time"]) for p in packets if p.get("dts_time") not in (None, "N/A")]
        sizes = [int(p["size"]) for p in packets if p.get("dts_time") not in (None, "N/A")]
        rates["video"] = peak_to_average(times, sizes)
        if sizes:
            rates["frames"] = (max(sizes), sum(sizes) / len(sizes))

    # The whole stream over its PCRs: bytes per packet, time interpolated from the PCRs.
    pcrs = ts["pcrs"]
    if len(pcrs) >= 2:
        times = [v / 90000 for _, v in pcrs]
        sizes = [(n - m) * PACKET for (m, _), (n, _) in zip([(0, 0)] + pcrs[:-1], pcrs)]
        rates["ts"] = peak_to_average(times, sizes)

    lines = [f"### {os.path.basename(a.file)}", "", "| | Check | Result |", "|---|---|---|"]
    for ok, what, detail in results:
        lines.append(f"| {'pass' if ok else 'FAIL'} | {what} | {detail} |")
    lines.append("")
    for name, label in (("video", "H.264 elementary stream"), ("ts", "transport stream")):
        avg, peak, ratio = rates.get(name, (None, None, None))
        if ratio:
            lines.append(f"- Peak-to-average bitrate, {label}, 1 s windows: **{ratio:.2f}** "
                         f"(peak {peak / 1000:.0f} kbps, average {avg / 1000:.0f} kbps)")
    if "frames" in rates:
        biggest, mean = rates["frames"]
        lines.append(f"- Largest video frame {biggest} bytes, mean {mean:.0f} bytes ({biggest / mean:.1f}x)")
    report = "\n".join(lines) + "\n"
    print(report)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(report + "\n")
    return 0 if all(ok for ok, _, _ in results) else 1


if __name__ == "__main__":
    sys.exit(main())
