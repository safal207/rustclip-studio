#!/usr/bin/env python3
"""Original, deterministic ambient jazz. No recordings, soundfonts, or model APIs.

python compose.py --minutes 60 --output quiet-window.wav --seed 207
Requires numpy, scipy and FFmpeg. The final file is stereo 44.1 kHz PCM16 WAV.
The additive/modal electric piano, bass and pad are synthesized from equations.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import time

import numpy as np
from scipy import signal
from scipy.fft import rfft, irfft, next_fast_len


SR = 44100
TEMPO = 64.0
BAR = 240.0 / TEMPO
CHUNK_SECONDS = 10.0
FADE_SECONDS = 17.0
TITLE = "Quiet Window — original ambient jazz"
KEYS = [
    ("C major", 48, [0, 2, 4, 5, 7, 9, 11]),
    ("A minor", 45, [0, 2, 3, 5, 7, 8, 10]),
    ("F major", 41, [0, 2, 4, 5, 7, 9, 11]),
    ("D minor", 38, [0, 2, 3, 5, 7, 8, 10]),
    ("G major", 43, [0, 2, 4, 5, 7, 9, 11]),
    ("E minor", 40, [0, 2, 3, 5, 7, 8, 10]),
    ("C major", 48, [0, 2, 4, 5, 7, 9, 11]),
    ("A minor", 45, [0, 2, 3, 5, 7, 8, 10]),
    ("F major", 41, [0, 2, 4, 5, 7, 9, 11]),
    ("B-flat major", 46, [0, 2, 4, 5, 7, 9, 11]),
    ("G minor", 43, [0, 2, 3, 5, 7, 8, 10]),
    ("C major", 48, [0, 2, 4, 5, 7, 9, 11]),
]
MAJOR = [[0, 5, 1, 4], [3, 2, 5, 1], [0, 2, 3, 4], [5, 3, 0, 4], [1, 4, 2, 5]]
MINOR = [[0, 5, 3, 6], [0, 3, 5, 4], [5, 3, 0, 6], [0, 6, 5, 4]]


def run(command: list[str], **kwargs):
    result = subprocess.run(command, text=True, capture_output=True, **kwargs)
    if result.returncode:
        raise RuntimeError(f"Command failed: {' '.join(command)}\n{result.stderr[-4000:]}")
    return result


def hash_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for data in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(data)
    return hasher.hexdigest()


def scale_note(tonic: int, scale: list[int], degree: int) -> int:
    octave, within = divmod(degree, 7)
    return tonic + 12 * octave + scale[within]


def in_range(note: int, lower: int, upper: int) -> int:
    while note < lower:
        note += 12
    while note > upper:
        note -= 12
    return note


def make_score(seconds: float, seed: int):
    """A 5-minute harmonic chapter; phrases, voicings and dynamics evolve."""
    rng = np.random.default_rng(seed)
    score = []
    chapters = []
    last_melody = 72
    previous_voicing = [57, 64, 69, 74]
    previous_key = None
    for bar in range(math.ceil(seconds / BAR)):
        start = bar * BAR
        chapter_index = int(start // 300)
        key_name, tonic, scale = KEYS[chapter_index % len(KEYS)]
        local_bar = bar % 80
        if key_name != previous_key or local_bar == 0:
            chapters.append({"start_seconds": round(start, 6), "title": key_name + " / quiet variations", "tempo_bpm": TEMPO})
            previous_key = key_name
        # Re-select eight-bar progressions, with no copied audio blocks.
        progression_rng = np.random.default_rng(seed + chapter_index * 7001 + (local_bar // 8) * 101)
        choices = MINOR if "minor" in key_name else MAJOR
        progression = choices[int(progression_rng.integers(len(choices)))]
        degree = progression[(local_bar // 2) % 4]
        chord = [scale_note(tonic, scale, degree + interval) for interval in [0, 2, 4, 6, 8]]
        # Gentle rootless voicings: third, seventh, ninth and fifth.
        raw_voicing = [chord[1], chord[3], chord[4], chord[2]]
        voicing = []
        for index, note in enumerate(raw_voicing):
            target = previous_voicing[index]
            candidates = [note + 12 * octave for octave in range(-1, 4) if 53 <= note + 12 * octave <= 77]
            nearest = min(candidates, key=lambda value: abs(value - target))
            voicing.append(nearest)
        previous_voicing = voicing
        chapter_energy = [.58, .52, .66, .46, .61, .48, .64, .54, .62, .45, .53, .48][chapter_index % 12]
        # A breath every 16 bars and longer rests around chapter transitions.
        breath = 0.72 if local_bar % 16 >= 12 else 1.0
        if local_bar < 2:
            breath *= .8
        velocity = float(chapter_energy * breath * rng.uniform(.87, 1.10))
        if bar % 2 == 0:
            ordered = sorted(set(voicing))
            for index, note in enumerate(ordered):
                timing = start + .12 + index * .052 + float(rng.uniform(-.010, .012))
                score.append((timing, "piano", note, velocity * .73, float(rng.uniform(-.28, .28)), int(rng.integers(3))))
            root = in_range(chord[0], 32, 45)
            score.append((start + .08, "bass", root, velocity * .52, -.08, 0))
            for note in [in_range(chord[1], 57, 69), in_range(chord[3], 62, 74)]:
                score.append((start, "pad", note, velocity * .19, .15 if note % 2 else -.15, 0))
        elif rng.random() < .43:
            # An occasional soft answer, not a repeating chord ostinato.
            note = sorted(voicing)[int(rng.integers(len(voicing)))]
            score.append((start + BAR * .56, "piano", note, velocity * .33, float(rng.uniform(-.16, .16)), 1))
        # Four-bar melodic sentences, with two to four-bar space between them.
        if local_bar % 8 < 4 and rng.random() < .55 and local_bar > 1:
            count = 2 if rng.random() < .73 else 3
            beat_positions = sorted(rng.choice(np.array([.25, 1.00, 1.75, 2.50, 3.25]), count, replace=False))
            targets = [in_range(note, 65, 81) for note in chord[1:]]
            for phrase_index, beat in enumerate(beat_positions):
                weighted = sorted(targets, key=lambda note: abs(note - last_melody))
                note = weighted[int(rng.choice([0, 1, 2], p=[.62, .28, .10]))]
                if phrase_index == 0 and rng.random() < .18:
                    note = weighted[-1]
                last_melody = note
                timing = start + float(beat) * 60 / TEMPO + float(rng.uniform(-.015, .018))
                score.append((timing, "piano", note, velocity * rng.uniform(.48, .68), float(rng.uniform(-.13, .13)), int(rng.integers(3))))
    # Leave a final settling window; sustained tails fade smoothly.
    score = [event for event in score if event[0] < seconds - min(4.5, seconds * .08)]
    score.sort(key=lambda event: event[0])
    return score, chapters


def synth_note(kind: str, midi: int, variation: int) -> np.ndarray:
    frequency = 440.0 * 2 ** ((midi - 69) / 12)
    duration = 7.0 if kind == "pad" else 5.5 if kind == "bass" else 6.0
    t = np.arange(round(duration * SR), dtype=np.float32) / SR
    if kind == "piano":
        # Slight inharmonicity and faster decay of high modes imitate a damped
        # electric piano. Gentle FM in the attack adds a soft struck character.
        out = np.zeros_like(t)
        coefficients = [1., .31, .105, .049, .021, .009]
        for harmonic, amplitude in enumerate(coefficients, 1):
            stretch = math.sqrt(1 + .000065 * harmonic * harmonic)
            decay = (2.9 * (62 / max(midi, 38)) ** 1.15) / (1 + .48 * (harmonic - 1))
            timbre = 1.0 + (variation - 1) * .035 * (harmonic - 1)
            out += amplitude * timbre * np.exp(-t / decay) * np.sin(2 * np.pi * frequency * harmonic * stretch * t)
        index = (0.20 + .018 * variation) * np.exp(-t / .20)
        out += .09 * np.exp(-t / 1.1) * np.sin(2 * np.pi * frequency * t + index * np.sin(2 * np.pi * frequency * 3.0 * t))
        attack = .011
        level = .19
    elif kind == "bass":
        out = np.exp(-t / 2.2) * (np.sin(2 * np.pi * frequency * t) + .18 * np.sin(4 * np.pi * frequency * t) + .055 * np.sin(6 * np.pi * frequency * t))
        attack = .032
        level = .21
    else:
        # Quiet detuned pad made from the same original oscillators.
        envelope = np.minimum(t / 1.5, 1) * np.minimum((duration - t) / 1.8, 1)
        out = envelope * (np.sin(2 * np.pi * frequency * .9992 * t) + np.sin(2 * np.pi * frequency * 1.0008 * t) + .12 * np.sin(4 * np.pi * frequency * t))
        attack = .025
        level = .13
    rise = np.minimum(t / attack, 1)
    rise = rise * rise * (3 - 2 * rise)
    release = np.minimum((duration - t) / .60, 1)
    release = np.clip(release, 0, 1)
    release = release * release * (3 - 2 * release)
    return np.asarray(out * rise * release * level, dtype=np.float32)


class Reverb:
    """Continuous stereo overlap-add convolution. No chunk resets or seams."""
    def __init__(self, block_size: int, seed: int):
        rng = np.random.default_rng(seed + 91071)
        ir_size = int(3.5 * SR)
        t = np.arange(ir_size) / SR
        self.block_size = block_size
        self.fft_size = next_fast_len(block_size + ir_size - 1)
        impulses = []
        for side in range(2):
            noise = rng.standard_normal(ir_size).astype(np.float32)
            noise = signal.sosfilt(signal.butter(2, 2900, fs=SR, output="sos"), noise).astype(np.float32)
            noise *= np.exp(-t / .70) * np.minimum(t / .06, 1)
            noise[:int(.026 * SR)] = 0
            noise[-int(.15 * SR):] *= np.linspace(1, 0, int(.15 * SR))
            noise /= max(np.sqrt(np.sum(noise * noise)), 1e-9)
            impulses.append(noise * .24)
        self.kernel = rfft(np.stack(impulses), self.fft_size, axis=1)
        self.tail = np.zeros((2, ir_size - 1), dtype=np.float32)
        self.ir_size = ir_size

    def process(self, dry: np.ndarray) -> np.ndarray:
        n = dry.shape[0]
        mono = np.mean(dry, axis=1)
        convolved = irfft(rfft(mono, self.fft_size)[None, :] * self.kernel, self.fft_size, axis=1)
        convolved[:, :self.ir_size - 1] += self.tail
        self.tail = np.asarray(convolved[:, n:n + self.ir_size - 1], dtype=np.float32).copy()
        return np.asarray(dry + convolved[:, :n].T, dtype=np.float32)


def render_raw(path: Path, seconds: float, seed: int, score):
    samples = round(seconds * SR)
    block_size = round(CHUNK_SECONDS * SR)
    cache = {}
    reverb = Reverb(block_size, seed)
    active = []
    event_index = 0
    peak = 0.0
    square_sum = 0.0
    clipped = 0
    command = ["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-f", "f32le", "-ar", str(SR), "-ac", "2", "-i", "pipe:0", "-c:a", "pcm_f32le", "-rf64", "auto", str(path)]
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        for block_start in range(0, samples, block_size):
            n = min(block_size, samples - block_start)
            end = block_start + n
            dry = np.zeros((n, 2), dtype=np.float32)
            while event_index < len(score) and round(score[event_index][0] * SR) < end:
                onset, kind, midi, velocity, pan, variation = score[event_index]
                key = (kind, midi, variation)
                if key not in cache:
                    cache[key] = synth_note(*key)
                start = round(onset * SR)
                left = math.sqrt((1 - pan) / 2)
                right = math.sqrt((1 + pan) / 2)
                active.append((start, cache[key], velocity, left, right))
                event_index += 1
            retained = []
            for onset, wave, gain, left, right in active:
                source_start = max(block_start - onset, 0)
                target_start = max(onset - block_start, 0)
                count = min(len(wave) - source_start, n - target_start)
                if count > 0:
                    segment = wave[source_start:source_start + count] * gain
                    dry[target_start:target_start + count, 0] += segment * left
                    dry[target_start:target_start + count, 1] += segment * right
                if onset + len(wave) > end:
                    retained.append((onset, wave, gain, left, right))
            active = retained
            mixed = reverb.process(dry)
            absolute_t = (block_start + np.arange(n, dtype=np.float32)) / SR
            fade_time = min(FADE_SECONDS, seconds / 4)
            envelope = np.minimum(absolute_t / fade_time, (seconds - absolute_t) / fade_time)
            envelope = np.clip(envelope, 0, 1)
            envelope = np.sin(envelope * np.pi / 2) ** 2
            mixed *= envelope[:, None]
            peak = max(peak, float(np.max(np.abs(mixed))))
            square_sum += float(np.sum(mixed.astype(np.float64) ** 2))
            clipped += int(np.count_nonzero(np.abs(mixed) >= 1.0))
            process.stdin.write(mixed.astype("<f4", copy=False).tobytes())
            if block_start % (60 * SR) == 0:
                print(f"Rendered {block_start / SR:.0f}/{seconds:.0f} seconds", flush=True)
        process.stdin.close()
        error = process.stderr.read().decode()
        code = process.wait()
        if code:
            raise RuntimeError(error)
    finally:
        if process.poll() is None:
            process.kill()
    return {"raw_sample_peak": peak, "raw_rms_dbfs": 20 * math.log10(max(math.sqrt(square_sum / (samples * 2)), 1e-12)), "raw_samples_at_or_above_full_scale": clipped, "cached_original_voices": len(cache)}


def loudness(path: Path, target: float = -19):
    result = run(["ffmpeg", "-hide_banner", "-i", str(path), "-af", f"loudnorm=I={target}:TP=-2:LRA=7:print_format=json", "-f", "null", "-"])
    matches = re.findall(r'\{\s*"input_i".*?\}', result.stderr, flags=re.S)
    if not matches:
        raise RuntimeError("FFmpeg loudness measurements missing")
    return json.loads(matches[-1])


def normalize(raw: Path, final: Path, measured: dict, target: float):
    filters = f"loudnorm=I={target}:TP=-2:LRA=7:measured_I={measured['input_i']}:measured_TP={measured['input_tp']}:measured_LRA={measured['input_lra']}:measured_thresh={measured['input_thresh']}:offset={measured['target_offset']}:linear=true:print_format=json"
    command = ["ffmpeg", "-y", "-hide_banner", "-i", str(raw), "-af", filters, "-ar", str(SR), "-ac", "2", "-c:a", "pcm_s16le", "-rf64", "auto", str(final)]
    result = run(command)
    matches = re.findall(r'\{\s*"input_i".*?\}', result.stderr, flags=re.S)
    return json.loads(matches[-1]) if matches else {}


def waveform_checks(path: Path):
    # A separate PCM scan verifies clipping and chunk joins on the final audio.
    process = subprocess.Popen(["ffmpeg", "-v", "error", "-i", str(path), "-f", "f32le", "-acodec", "pcm_f32le", "pipe:1"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    max_peak = 0.0
    clipped = 0
    all_step_peak = 0.0
    join_step_peak = 0.0
    count = 0
    previous = None
    while True:
        data = process.stdout.read(int(CHUNK_SECONDS * SR) * 2 * 4)
        if not data:
            break
        waveform = np.frombuffer(data, dtype="<f4").reshape(-1, 2)
        max_peak = max(max_peak, float(np.max(np.abs(waveform))))
        clipped += int(np.count_nonzero(np.abs(waveform) >= .999))
        all_step_peak = max(all_step_peak, float(np.max(np.abs(np.diff(waveform, axis=0)))))
        if previous is not None:
            join_step_peak = max(join_step_peak, float(np.max(np.abs(waveform[0] - previous))))
        previous = waveform[-1].copy()
        count += len(waveform)
    error = process.stderr.read().decode()
    if process.wait():
        raise RuntimeError(error)
    return {"decoded_samples_per_channel": count, "sample_peak_dbfs": 20 * math.log10(max(max_peak, 1e-12)), "samples_at_or_above_minus_0_009_dbfs": clipped, "largest_adjacent_sample_step": all_step_peak, "largest_10_second_chunk_join_step": join_step_peak, "join_step_within_natural_signal_range": join_step_peak <= all_step_peak + 1e-7, "decoder_errors": error.strip()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--minutes", type=float, default=60)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seed", type=int, default=207)
    parser.add_argument("--target-lufs", type=float, default=-19)
    parser.add_argument("--keep-raw", action="store_true")
    args = parser.parse_args()
    if not 1 <= args.minutes <= 480:
        parser.error("minutes must be between 1 and 480")
    if args.output.suffix.lower() != ".wav":
        parser.error("output must end in .wav")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    raw = args.output.with_suffix(".raw.wav")
    seconds = args.minutes * 60
    started = time.monotonic()
    score, chapters = make_score(seconds, args.seed)
    raw_metrics = render_raw(raw, seconds, args.seed, score)
    measured = loudness(raw, args.target_lufs)
    normalization = normalize(raw, args.output, measured, args.target_lufs)
    checks = waveform_checks(args.output)
    final_loudness = loudness(args.output, args.target_lufs)
    preview = args.output.with_name(args.output.stem + "-preview.mp3")
    preview_seconds = min(60, seconds)
    run(["ffmpeg", "-y", "-v", "error", "-i", str(args.output), "-t", str(preview_seconds), "-af", f"afade=t=out:st={max(0, preview_seconds - 3)}:d=3", "-c:a", "libmp3lame", "-b:a", "192k", str(preview)])
    manifest = {
        "title": TITLE,
        "duration_seconds": seconds,
        "sample_rate_hz": SR,
        "channels": 2,
        "output_format": "PCM16 WAV / RF64 when needed",
        "seed": args.seed,
        "tempo_bpm": TEMPO,
        "fade_in_seconds": min(FADE_SECONDS, seconds / 4),
        "fade_out_seconds": min(FADE_SECONDS, seconds / 4),
        "composition_event_count": len(score),
        "chapters": chapters,
        "method": "Original modal/additive electric piano, sine-harmonic bass, detuned pad and continuous stereo convolution reverb; random choices seeded; freshly scheduled harmonic and melodic variations throughout.",
        "rights": {
            "audio_source": "All notes and room impulse responses synthesized by this generator; no recordings, sample packs, soundfonts or existing melodies supplied.",
            "code_license": "MIT",
            "third_party_dependencies": {"numpy": "BSD-3-Clause", "scipy": "BSD-3-Clause", "FFmpeg": "license depends on installed build; executable used for encoding/measurement, not embedded in generated media"},
            "limitations": "Synthetic instruments, not an acoustic performance. Automated generation cannot guarantee copyright exclusivity, Content ID eligibility or platform monetization approval. No therapeutic claims."
        },
        "quality": {**raw_metrics, **checks, "integrated_lufs": float(final_loudness["input_i"]), "true_peak_dbtp": float(final_loudness["input_tp"]), "loudness_range_lu": float(final_loudness["input_lra"]), "normalization_type": normalization.get("normalization_type"), "ffmpeg_loudness": final_loudness},
        "files": {"audio": args.output.name, "audio_sha256": hash_file(args.output), "preview": preview.name, "preview_sha256": hash_file(preview)},
        "generator_sha256": hash_file(Path(__file__)),
        "render_wall_seconds": round(time.monotonic() - started, 3),
        "reproduce": f"python compose.py --minutes {args.minutes:g} --output {args.output.name} --seed {args.seed} --target-lufs {args.target_lufs:g}"
    }
    manifest_path = args.output.with_suffix(".json")
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    if not args.keep_raw:
        raw.unlink()
    print(json.dumps({"audio": str(args.output), "preview": str(preview), "manifest": str(manifest_path), "quality": manifest["quality"], "render_wall_seconds": manifest["render_wall_seconds"]}, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
