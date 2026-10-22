#!/usr/bin/env python3
"""Bridge between Pantheon's local STT backends and on-device engines.

Every supported engine is reached through a Python API - Moondream's
Photon ships no CLI at all - so the engine call lives here and the Rust
side keeps what it is good at: argument handling, the wall-clock bound,
audio normalisation, and parsing of the engine's own result.

The program is read from stdin (Pantheon runs `python3 - <args>`), so
nothing has to be installed on disk.

Contract: the engine's result object is printed verbatim on a single
line, prefixed with MARKER. Library chatter on stdout (progress bars,
download logs) is therefore ignored, and the Rust side parses the real
engine wire format rather than a format of our own.

Usage:
    python3 - --engine <whistle|cactus|photon> --audio <file.wav>
              [--model ID] [--weights PATH] [--language CODE]
              [--prompt TEXT] [--device cpu|cuda|mps|auto]
"""

import argparse
import json
import sys

MARKER = "__PANTHEON_STT__"


def emit(payload):
    """Print the engine's own result object for the Rust side to parse."""
    sys.stdout.write("%s %s\n" % (MARKER, json.dumps(payload)))
    sys.stdout.flush()


def _is_parakeet(model):
    return "parakeet" in (model or "").lower()


def run_whistle(args):
    """Whistle on the Cactus/Needle engine: 16.9 MB, CPU only.

    `weights` is a local .cact file; left unset, the published weights
    are used (downloaded on first run). The prompt hint maps onto
    Whistle's keyword biasing, the closest equivalent it has.
    """
    import needle

    kwargs = {}
    if args.language:
        kwargs["language"] = args.language
    if args.weights:
        kwargs["weights"] = args.weights
    if args.prompt:
        keywords = [w.strip() for w in args.prompt.split(",") if w.strip()]
        if keywords:
            kwargs["keywords"] = keywords
    emit(needle.transcribe(args.audio, **kwargs))


def run_cactus(args):
    """NVIDIA Parakeet on the Cactus engine.

    `ensure_model` fetches the prebuilt bundle on first use and returns
    its directory; the handle is released even when transcription fails.
    The prompt hint is Whisper-family only, so it is not sent to
    Parakeet.
    """
    import cactus

    bundle = cactus.ensure_model(args.model)
    handle = cactus.cactus_init(str(bundle), None, False)
    try:
        options = {"language": args.language} if args.language else None
        prompt = args.prompt if not _is_parakeet(args.model) else None
        emit(cactus.cactus_transcribe(handle, args.audio, prompt or None, options))
    finally:
        cactus.cactus_destroy(handle)


def run_photon(args):
    """Moondream Photon: compiled on-device ASR for NVIDIA GPUs and
    Apple Silicon (the Parakeet Redux checkpoint also runs on CPU)."""
    import moondream as md

    kwargs = {}
    if args.device and args.device != "auto":
        kwargs["device"] = args.device
    with md.photon(args.model, **kwargs) as speech:
        transcribe = {}
        # Parakeet detects its language itself and rejects a forced
        # one; the Whisper and Qwen3 checkpoints accept both.
        if args.language and not _is_parakeet(args.model):
            transcribe["language"] = args.language
        if args.prompt and not _is_parakeet(args.model):
            transcribe["initial_prompt"] = args.prompt
        emit(speech.transcribe(audio=args.audio, **transcribe))


ENGINES = {"whistle": run_whistle, "cactus": run_cactus, "photon": run_photon}


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", required=True, choices=sorted(ENGINES))
    parser.add_argument("--audio", required=True)
    parser.add_argument("--model", default="")
    parser.add_argument("--weights", default="")
    parser.add_argument("--language", default="")
    parser.add_argument("--prompt", default="")
    parser.add_argument("--device", default="auto")
    args = parser.parse_args(argv)
    ENGINES[args.engine](args)


if __name__ == "__main__":
    main(sys.argv[1:])
