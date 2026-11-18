"""A stand-in for DeepgramFluxSTTService, for tests only.

Selected by MEETILY_SIDECAR_FAKE_FLUX=1 (the Rust process tests set it), so the
whole sidecar can be exercised end to end without a Deepgram key or network.
Every 0.1 s of audio (3200 bytes) is one committed turn, reported the way the
real service reports it: an InterimTranscriptionFrame for the turn's Update,
then a TranscriptionFrame whose `result` is a Flux TurnInfo, then the
on_end_of_turn event.
"""

from __future__ import annotations

from pipecat.frames.frames import InputAudioRawFrame, InterimTranscriptionFrame, TranscriptionFrame
from pipecat.processors.frame_processor import FrameProcessor

BYTES_PER_TURN = 3200


class FakeFlux(FrameProcessor):
    def __init__(self, fail_after: int | None = None, **kwargs):
        super().__init__(**kwargs)
        for name in ("on_end_of_turn", "on_eager_end_of_turn", "on_turn_resumed"):
            self._register_event_handler(name)
        self._bytes = 0
        self._turns = 0
        self._fail_after = fail_after

    async def process_frame(self, frame, direction):
        await super().process_frame(frame, direction)
        if not isinstance(frame, InputAudioRawFrame):
            await self.push_frame(frame, direction)
            return
        self._bytes += len(frame.audio)
        while self._bytes >= BYTES_PER_TURN:
            self._bytes -= BYTES_PER_TURN
            self._turns += 1
            if self._fail_after is not None and self._turns > self._fail_after:
                await self.push_error("Flux refused the key", force_treat_as_permanent=True)
                return
            start = (self._turns - 1) * 0.1
            text = f"turn {self._turns}"
            result = {
                "type": "TurnInfo",
                "event": "EndOfTurn",
                "transcript": text,
                "audio_window_start": start,
                "audio_window_end": start + 0.1,
                "words": [
                    {"word": "turn", "confidence": 0.9, "start": start, "end": start + 0.05},
                    {"word": str(self._turns), "confidence": 0.9, "start": start + 0.05, "end": start + 0.1},
                ],
            }
            update = {**result, "event": "Update", "transcript": "turn"}
            await self.push_frame(InterimTranscriptionFrame("turn", "", "", result=update))
            await self.push_frame(TranscriptionFrame(text, "", "", result=result, finalized=True))
            await self._call_event_handler("on_end_of_turn", text)
