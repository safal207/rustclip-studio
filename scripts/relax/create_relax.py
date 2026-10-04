#!/usr/bin/env python3
"""Original long-form music video; no account access or automatic publication."""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import uuid


def now() -> str:
    return datetime.now(timezone.utc).isoformat()


def digest(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def run(args: list[str], capture: bool = False) -> str:
    result = subprocess.run(args, check=True, text=True,
                            stdout=subprocess.PIPE if capture else None,
                            stderr=subprocess.PIPE if capture else None)
    return (result.stdout or "") + (result.stderr or "")


def probe(path: Path) -> dict:
    return json.loads(run(["ffprobe", "-v", "error", "-show_streams",
                           "-show_format", "-of", "json", str(path)], True))


def write_json(path: Path, value: object) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2,
                                    allow_nan=False) + "\n", encoding="utf-8")
    temporary.replace(path)


class Journal:
    """Compatible with rustclip_studio::graph; append order is not causality."""
    def __init__(self, path: Path, subject: str, seconds: int, seed: int):
        self.path = path
        self.subject = subject
        self.seconds = seconds
        self.seed = seed
        self.events: list[dict] = []

    def append(self, before: str, after: str, reason: str, observed: dict,
               parents: list[dict] | None = None, claim: str = "OBSERVATION") -> dict:
        timestamp = now()
        event = {
            "seq": len(self.events) + 1, "id": str(uuid.uuid4()),
            "subject_id": self.subject, "operation_id": str(uuid.uuid4()),
            "execution_id": str(uuid.uuid4()), "valid_at": timestamp,
            "recorded_at": timestamp, "from_state": before, "to_state": after,
            "reason": reason, "claim_level": claim, "spatial_scope": "local",
            "space": {"pipeline": "relax", "duration_seconds": self.seconds,
                      "seed": self.seed}, "confidence": None,
            "parents": [{"id": p["id"], "hash": p["hash"],
                         "relation": "dependency"} for p in (parents or [])],
            "supersedes": None, "expected": {}, "observed": observed,
            "evidence": {}, "previous_hash": self.events[-1]["hash"]
            if self.events else "0" * 64, "hash": "",
        }
        encoded = json.dumps(event, ensure_ascii=False, sort_keys=True,
                             separators=(",", ":"), allow_nan=False).encode("utf-8")
        event["hash"] = hashlib.sha256(encoded).hexdigest()
        self.events.append(event)
        write_json(self.path, self.events)
        return event


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--minutes", type=int, default=60)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seed", type=int, default=207)
    # Explicit reuse avoids recomposing a finished master during packaging.
    parser.add_argument("--audio-source", type=Path)
    parser.add_argument("--visual-source", type=Path)
    parser.add_argument("--poster-source", type=Path)
    options = parser.parse_args()
    if not 1 <= options.minutes <= 480:
        parser.error("--minutes must be between 1 and 480")
    if not 0 <= options.seed <= 2**64 - 1:
        parser.error("--seed must be an unsigned 64-bit integer")
    output = options.output.expanduser().resolve()
    if output.suffix.lower() != ".mp4":
        parser.error("--output must end in .mp4")
    if output.exists():
        parser.error("output already exists; choose a new filename")
    for executable in ["ffmpeg", "ffprobe"]:
        if not shutil.which(executable):
            parser.error(f"{executable} is required")
    try:
        import numpy  # noqa: F401
        import scipy  # noqa: F401
        import PIL  # noqa: F401
    except ImportError as error:
        parser.error(f"install scripts/relax/requirements.txt: {error}")
    output.parent.mkdir(parents=True, exist_ok=True)
    work = output.with_suffix(".work")
    work.mkdir(exist_ok=False)
    seconds = options.minutes * 60
    journal = Journal(output.with_suffix(".causal-graph.json"), output.name,
                      seconds, options.seed)
    intent = journal.append("idea", "planned",
                            "Original relaxation music with an illustrated background",
                            {"title": "Evening by the Sea", "voice": False,
                             "youtube_uploaded": False, "revenue_claimed": False},
                            claim="INTENT")
    scripts = Path(__file__).resolve().parent
    try:
        if options.audio_source:
            audio = options.audio_source.resolve()
        else:
            audio = work / "music.wav"
            run([sys.executable, str(scripts / "compose.py"), "--minutes",
                 str(options.minutes), "--output", str(audio), "--seed", str(options.seed)])
        audio_info = probe(audio)
        audio_streams = [s for s in audio_info["streams"] if s["codec_type"] == "audio"]
        if not audio_streams or float(audio_info["format"]["duration"]) + 0.01 < seconds:
            raise ValueError("audio must contain a full-length audio stream")
        audio_event = journal.append("planned", "music_created",
                                     "Original composition rendered without third-party recordings",
                                     {"file": audio.name, "sha256": digest(audio)}, [intent])
        if options.visual_source:
            visual = options.visual_source.resolve()
        else:
            visual_dir = work / "visual"
            run([sys.executable, str(scripts / "visual.py"), "--output-dir",
                 str(visual_dir)])
            visual = visual_dir / "coastal-room-loop-90s.mp4"
        visual_info = probe(visual)
        video_streams = [s for s in visual_info["streams"] if s["codec_type"] == "video"]
        if not video_streams or any(s["codec_type"] == "audio" for s in visual_info["streams"]):
            raise ValueError("visual must be a video-only loop")
        visual_event = journal.append("planned", "visual_created",
                                      "Original animated illustration; the visual loop repeats",
                                      {"file": visual.name, "sha256": digest(visual)}, [intent])
        temporary = work / "assembled.mp4"
        run(["ffmpeg", "-hide_banner", "-loglevel", "warning", "-nostdin", "-y",
             "-stream_loop", "-1", "-i", str(visual), "-i", str(audio),
             "-map", "0:v:0", "-map", "1:a:0", "-t", str(seconds),
             "-c:v", "copy", "-c:a", "aac", "-b:a", "192k", "-ar", "48000",
             "-movflags", "+faststart", "-metadata", "title=Evening by the Sea",
             "-metadata", "comment=Original procedural music and animated illustration; RustClip Studio",
             str(temporary)])
        assembly = journal.append("assets_ready", "assembled",
                                  "Full-length composition and repeated visual loop muxed into MP4",
                                  {"duration_seconds": seconds}, [audio_event, visual_event])
        media_info = probe(temporary)
        actual = float(media_info["format"]["duration"])
        if abs(actual - seconds) > 0.25:
            raise ValueError(f"unexpected duration {actual}; expected {seconds}")
        # Decode every packet. This also catches corruption beyond a short preview.
        run(["ffmpeg", "-hide_banner", "-v", "error", "-xerror", "-nostdin",
             "-i", str(temporary), "-map", "0:v:0", "-map", "0:a:0",
             "-f", "null", "-"])
        level_log = run(["ffmpeg", "-hide_banner", "-nostdin", "-i", str(temporary),
                         "-vn", "-af", "volumedetect", "-f", "null", "-"], True)
        level = re.search(r"max_volume:\s*(-?[\d.]+) dB", level_log)
        if not level or float(level.group(1)) >= -0.1:
            raise ValueError("audio peak measurement missing or too close to digital clipping")
        temporary.replace(output)
        output_hash = digest(output)
        verified = journal.append("assembled", "verified",
                                  "Full audio/video decoded; duration and peak checked",
                                  {"file": output.name, "sha256": output_hash,
                                   "max_volume_dbfs": level.group(1),
                                   "duration_seconds": str(actual)}, [assembly])
        preview = output.with_suffix(".preview.mp4")
        run(["ffmpeg", "-hide_banner", "-loglevel", "warning", "-nostdin", "-y",
             "-i", str(output), "-t", "60", "-map", "0:v:0", "-map", "0:a:0",
             "-c", "copy", "-movflags", "+faststart", str(preview)])
        poster = output.with_suffix(".poster.png")
        if options.poster_source:
            shutil.copyfile(options.poster_source.resolve(), poster)
        else:
            run(["ffmpeg", "-hide_banner", "-loglevel", "warning", "-nostdin", "-y",
                 "-ss", "10", "-i", str(output), "-frames:v", "1", "-update", "1", str(poster)])
        write_json(output.with_suffix(".verification.json"), {
            "schema": "rustclip-relax-v1", "created_at": now(), "seed": options.seed,
            "title": "Evening by the Sea", "duration_seconds": actual,
            "file": output.name, "size_bytes": output.stat().st_size,
            "sha256": output_hash, "ffprobe": media_info,
            "checks": {"full_decode": "passed", "duration": "passed",
                       "audio_max_volume_dbfs": float(level.group(1)),
                       "no_voice": True},
            "production": {"music": "Original procedural composition; no third-party samples",
                           "visual": "Original procedural animated illustration; repeated visual loop",
                           "visual_loop_seconds": float(visual_info["format"]["duration"]),
                           "not_a_live_recording": True, "not_a_medical_treatment": True,
                           "monetization_approved": False, "youtube_uploaded": False},
            "generator_sha256": {name: digest(scripts / name) for name in
                                 ["create_relax.py", "compose.py", "visual.py"]},
            "tool_versions": {"python": sys.version.split()[0],
                              "numpy": numpy.__version__, "scipy": scipy.__version__,
                              "pillow": PIL.__version__,
                              "ffmpeg": run(["ffmpeg", "-version"], True).splitlines()[0]},
            "journal_head": verified["hash"], "preview": preview.name, "poster": poster.name,
        })
        print(str(output), flush=True)
    except Exception as error:
        journal.append("in_progress", "failed", "Production stopped on a concrete error",
                       {"error": str(error)}, [journal.events[-1]])
        raise


if __name__ == "__main__":
    main()
