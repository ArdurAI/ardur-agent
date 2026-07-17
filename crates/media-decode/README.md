# ardur-media-decode

Phase 1 skeleton for media decode support shared by the audio transcription
plan and later video/media plans.

This crate intentionally does not spawn FFmpeg yet. It only owns the stable
format and decode request/result shapes that future sandboxed decode code will
consume.
