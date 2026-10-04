#!/usr/bin/env python3
"""Independent, bounded-memory PCM integrity and silent-gap check."""
import argparse
import hashlib
import json
from pathlib import Path
import wave

import numpy as np


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("audio", type=Path)
    parser.add_argument("--seconds", type=float, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    file_hash = hashlib.sha256()
    with args.audio.open("rb") as file:
        for data in iter(lambda: file.read(2 ** 20), b""):
            file_hash.update(data)
    expected_frames = None
    data_hash = hashlib.sha256()
    chunk_hashes = set()
    repeated_chunks = 0
    total = 0
    sum_squares = 0.
    sample_peak = 0
    exact_full_scale_samples = 0
    longest_internal_silence = 0
    trailing_zeros = 0
    minimum_internal_one_second_rms = 1.
    with wave.open(str(args.audio), "rb") as audio:
        rate = audio.getframerate()
        channels = audio.getnchannels()
        width = audio.getsampwidth()
        header_frames = audio.getnframes()
        if width != 2 or channels != 2:
            raise ValueError("Expected stereo PCM16 WAV")
        expected_frames = round(args.seconds * rate)
        while True:
            data = audio.readframes(rate)
            if not data:
                break
            data_hash.update(data)
            digest = hashlib.sha256(data).hexdigest()
            if digest in chunk_hashes:
                repeated_chunks += 1
            chunk_hashes.add(digest)
            pcm = np.frombuffer(data, dtype="<i2").reshape(-1, channels)
            absolute = np.abs(pcm.astype(np.int32))
            sample_peak = max(sample_peak, int(np.max(absolute)))
            exact_full_scale_samples += int(np.count_nonzero(absolute >= 32767))
            squared = pcm.astype(np.float64) ** 2
            sum_squares += float(squared.sum())
            if total >= 20 * rate and total + len(pcm) <= expected_frames - 20 * rate:
                minimum_internal_one_second_rms = min(minimum_internal_one_second_rms, float(np.sqrt(np.mean(squared)) / 32768))
                zero = np.all(pcm == 0, axis=1)
                bounds = np.flatnonzero(np.diff(np.r_[False, zero, False].astype(np.int8))).reshape(-1, 2)
                lengths = bounds[:, 1] - bounds[:, 0]
                if len(bounds) and bounds[0, 0] == 0:
                    lengths[0] += trailing_zeros
                if len(lengths):
                    longest_internal_silence = max(longest_internal_silence, int(max(lengths)))
                if len(bounds) and bounds[-1, 1] == len(pcm):
                    trailing_zeros = int(lengths[-1])
                else:
                    trailing_zeros = 0
            total += len(pcm)
    result = {
        "path": args.audio.name,
        "bytes": args.audio.stat().st_size,
        "sha256": file_hash.hexdigest(),
        "decoded_pcm_sha256": data_hash.hexdigest(),
        "sample_rate_hz": rate,
        "channels": channels,
        "sample_width_bytes": width,
        "expected_frames_per_channel": expected_frames,
        "header_frames_per_channel": header_frames,
        "read_frames_per_channel": total,
        "duration_matches": expected_frames == header_frames == total,
        "duration_seconds": total / rate,
        "exact_full_scale_samples": exact_full_scale_samples,
        "peak_dbfs": float(20 * np.log10(max(sample_peak / 32768, 1e-12))),
        "rms_dbfs": float(20 * np.log10(max(np.sqrt(sum_squares / (total * channels)) / 32768, 1e-12))),
        "minimum_internal_one_second_rms_dbfs": float(20 * np.log10(max(minimum_internal_one_second_rms, 1e-12))),
        "longest_internal_exact_zero_run_seconds": longest_internal_silence / rate,
        "unique_one_second_pcm_blocks": len(chunk_hashes),
        "duplicate_one_second_pcm_blocks": repeated_chunks,
        "note": "Exact zero-run scan excludes 20 seconds at each endpoint for fades; unique-block hashes prove no identical one-second PCM blocks in this file, not copyright exclusivity."
    }
    output = args.output or args.audio.with_suffix(".pcm-verification.json")
    output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    if not result["duration_matches"] or exact_full_scale_samples or longest_internal_silence > rate / 5:
        raise SystemExit("PCM integrity validation failed")


if __name__ == "__main__":
    main()
