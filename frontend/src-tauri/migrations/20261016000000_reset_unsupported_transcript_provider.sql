-- Earlier builds could store groq, openai or elevenLabs as the transcription
-- provider. None ever had an engine, and recording validation rejects them
-- (audio/transcription/engine.rs), so such a row leaves recording broken while
-- the settings UI no longer lists the value to switch away from. Move it to
-- the fresh-install default. The model id must equal
-- config::DEFAULT_PARAKEET_MODEL; a test in repositories/setting.rs checks it.
UPDATE transcript_settings
SET provider = 'parakeet', model = 'parakeet-tdt-0.6b-v3-int8'
WHERE provider NOT IN ('localWhisper', 'parakeet', 'deepgram');
