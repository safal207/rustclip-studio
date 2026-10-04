#!/usr/bin/env python3
"""Create a smaller download copy while retaining the full-quality master."""
from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
import shutil

from create_relax import Journal, digest, now, probe, run, write_json


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--audio-source", type=Path)
    parser.add_argument("--loop-source", type=Path)
    args = parser.parse_args()
    source, output = args.source.resolve(), args.output.resolve()
    if output.exists() or output.suffix.lower() != ".mp4":
        parser.error("choose a new .mp4 output filename")
    metadata = json.loads(source.with_suffix(".verification.json").read_text())
    if digest(source) != metadata["sha256"]:
        parser.error("source hash does not match its verification record")
    seconds = round(metadata["duration_seconds"])
    output.parent.mkdir(parents=True, exist_ok=True)
    work = output.with_suffix(".work")
    work.mkdir(exist_ok=False)
    journal = Journal(output.with_suffix(".causal-graph.json"), output.name,
                      seconds, metadata["seed"])
    journal.events = json.loads(source.with_suffix(".causal-graph.json").read_text())
    intent = journal.append("verified_master", "compact_copy_planned",
                            "Separate smaller download copy; original master retained",
                            {"source": source.name, "sha256": metadata["sha256"]},
                            [journal.events[-1]], claim="INTENT")
    if args.loop_source:
        loop = args.loop_source.resolve()
    else:
        loop = work / "loop.mp4"
        run(["ffmpeg", "-hide_banner", "-v", "warning", "-nostdin", "-y",
             "-i", str(source), "-t", "90", "-an", "-vf", "fps=4",
             "-c:v", "libx264", "-preset", "veryslow", "-crf", "28",
             "-pix_fmt", "yuv420p", "-g", "360", "-keyint_min", "360",
             "-sc_threshold", "0", "-movflags", "+faststart", str(loop)])
    loop_info = probe(loop)
    video_bytes = loop.stat().st_size * math.ceil(seconds / float(loop_info["format"]["duration"]))
    # Leave room for AAC bitrate variation and MP4 indexes below the 100 MiB
    # limit of this transfer route. This is not a limit of Google Drive itself.
    budget = 98 * 1024 * 1024 - video_bytes - 3 * 1024 * 1024
    bitrate = next((value for value in [96, 80, 64]
                    if seconds * value * 1000 / 8 < budget), None)
    if bitrate is None:
        raise ValueError("compact visual cannot meet the download size budget")
    audio = args.audio_source.resolve() if args.audio_source else source
    temporary = work / "assembled.mp4"
    run(["ffmpeg", "-hide_banner", "-v", "warning", "-nostdin", "-y",
         "-stream_loop", "-1", "-i", str(loop), "-i", str(audio),
         "-map", "0:v:0", "-map", "1:a:0", "-t", str(seconds),
         "-c:v", "copy", "-c:a", "aac", "-b:a", f"{bitrate}k", "-ar", "48000",
         "-movflags", "+faststart", "-metadata", "title=Evening by the Sea — compact copy",
         str(temporary)])
    encoded = journal.append("compact_copy_planned", "compact_copy_encoded",
                             "Same composition, lower frame rate and audio bitrate",
                             {"audio_bitrate_kbps": bitrate, "frame_rate": "4 fps",
                              "visual_sha256": digest(loop), "audio_input_sha256": digest(audio)},
                             [intent])
    media = probe(temporary)
    if abs(float(media["format"]["duration"]) - seconds) > 1.0:
        raise ValueError("compact duration mismatch")
    if temporary.stat().st_size >= 100 * 1024 * 1024:
        raise ValueError("compact file still exceeds the transfer limit")
    run(["ffmpeg", "-hide_banner", "-v", "error", "-xerror", "-nostdin",
         "-i", str(temporary), "-f", "null", "-"])
    temporary.replace(output)
    sha = digest(output)
    verified = journal.append("compact_copy_encoded", "verified",
                              "Entire compact audio and video decoded; size and duration checked",
                              {"file": output.name, "sha256": sha,
                               "size_bytes": output.stat().st_size}, [encoded])
    write_json(output.with_suffix(".verification.json"), {
        "schema": "rustclip-relax-compact-v1", "created_at": now(),
        "file": output.name, "size_bytes": output.stat().st_size, "sha256": sha,
        "duration_seconds": float(media["format"]["duration"]),
        "source_file": source.name, "source_sha256": metadata["sha256"],
        "seed": metadata["seed"], "journal_head": verified["hash"],
        "generator_sha256": digest(Path(__file__)), "ffprobe": media,
        "checks": {"full_decode": "passed", "size_below_100_mib": True},
        "quality": {"frame_rate": "4 fps", "audio_bitrate_kbps": bitrate,
                    "description": "Reduced download copy; keep the full master for best upload quality"},
    })
    poster = source.with_suffix(".poster.png")
    if poster.is_file():
        shutil.copyfile(poster, output.with_suffix(".poster.png"))
    print(str(output), flush=True)


if __name__ == "__main__":
    main()
