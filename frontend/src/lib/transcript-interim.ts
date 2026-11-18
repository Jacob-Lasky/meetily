// The not-yet-final words on each live channel (Deepgram interim results:
// nova `is_final: false`, Flux `Update`). Shown greyed under the transcript
// until that channel's next final replaces them. Display only: never stored,
// copied, exported or summarized.
//
// DO NOT change this name here alone: it MUST equal TRANSCRIPT_INTERIM_EVENT in
// src-tauri/src/audio/deepgram.rs (`ts_listens_for_transcript_interim` checks).
export const TRANSCRIPT_INTERIM_EVENT = 'transcript-interim';

export interface TranscriptInterim {
  /** Channel label ("Mic-In" / "Speaker-Out"), or null on an unlabelled stream. */
  speaker: string | null;
  /** The words so far; empty clears the channel. */
  text: string;
}

/** One channel's live line, in the order the channels started speaking. */
export interface InterimLine {
  speaker: string | null;
  text: string;
}

/**
 * The live lines after one interim event. A channel keeps its place while it
 * updates; a cleared channel is removed, and when it speaks again it goes
 * last, after whoever is still mid-sentence. Returns the same array when
 * nothing changed, so React skips the render.
 */
export function applyInterim(lines: InterimLine[], update: TranscriptInterim): InterimLine[] {
  const speaker = update.speaker ?? null;
  const text = update.text.trim();
  const index = lines.findIndex((line) => line.speaker === speaker);
  if (!text) {
    return index === -1 ? lines : lines.filter((_, i) => i !== index);
  }
  if (index === -1) {
    return [...lines, { speaker, text }];
  }
  if (lines[index].text === text) {
    return lines;
  }
  return lines.map((line, i) => (i === index ? { speaker, text } : line));
}
