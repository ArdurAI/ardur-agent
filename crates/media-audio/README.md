# ardur-media-audio

Audio capability crate for speech-to-text, text-to-speech, request validation,
cost envelopes, capability checks, and receipt-event vocabulary.

## Ready Surfaces

- `WhisperApiTranscriptionProvider` calls an OpenAI-compatible Whisper
  transcription API and is wrapped as the runtime tool `voice.transcribe`.
- `LocalSpeechToTextProvider` runs a configured local STT command directly
  without a shell for on-device engines such as whisper.cpp or Vosk.
- `LocalTextToSpeechProvider` runs a configured local TTS command directly
  without a shell for engines such as Piper or the host OS speech stack.
- `LocalTextToSpeechTool` exposes local TTS as `voice.speak` for registries that
  explicitly wire it.

The server auto-registers `voice.transcribe` when `OPENAI_WHISPER_API_KEY` or
`OPENAI_API_KEY` is present. Local STT/TTS providers are implemented and tested,
but they are not yet auto-registered by `ardur-server`.

## Configuration

Whisper transcription:

```sh
OPENAI_WHISPER_API_KEY=sk-...
OPENAI_WHISPER_BASE_URL=https://api.openai.com/v1
OPENAI_WHISPER_MODEL=whisper-1
```

Local providers:

```sh
ARDUR_LOCAL_STT_COMMAND=/usr/local/bin/whisper-local
ARDUR_LOCAL_STT_MODEL=base.en
ARDUR_LOCAL_TTS_COMMAND=/usr/local/bin/piper
ARDUR_LOCAL_TTS_MODEL=en_US
```

Local command execution is direct `exec`, not `/bin/sh -c`. Provider responses
are still bounded by the crate's request validation, scope checks, and
receipt-hash generation.
