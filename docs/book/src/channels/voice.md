# Voice & Telephony

The legacy voice transports (ClawdTalk SIP voice, carrier Voice Call, and
local Voice Wake) are RETIRED and their schemas are removed: the runners
went with the `zeroclaw-channels` crate (#579, #581) and the schemas were
deleted under [#584](https://github.com/kckylechen1/zeroclaw/issues/584)
Slice 3. Leftover sections tombstone at load with a structured warning.

## Retired voice-duplex event logger

The Gateway's `gateway-voice-duplex` feature, event logger and
`[channels.voice_duplex.<alias>]` configuration are retired under
[ADR-023 §8](../architecture/decisions/ADR-023-realtime-voice-carrier-with-body-delegation.md#8-scope).
The logger did not perform speech recognition, synthesis or turn cancellation.

Remove the old Cargo feature from build commands. Remove obsolete
`[channels.voice_duplex.*]` or `[channels_config.voice_duplex]` sections and
their env overrides after reviewing them. Existing sections produce a
`retired_channel_transport_removed` warning and cannot activate a transport,
bind a channel or become an outbound target. Their aliases, `enabled` values
and `excluded_tools` lists are not converted into voice-surface authority.

The former `speech_start`, `speech_end`, `barge_in`, `tts_cancel` and
`tts_chunk` frames follow the existing protocol rejection path; scoped
credentials may be refused earlier by their operation allowlist. These frames
do not submit a body turn, cancel work or record a transcript as owner input.

## Current voice surface

The default-off `voice-surface` feature presents spoken text through
`/ws/chat?surface=voice&voice_provider=<provider>`. It uses a scoped device
bridge credential and the existing voice consent and persona projection
checks. Audio transport belongs to the external adapter; the Node capability
path remains separately authorized.

See [ADR-023](../architecture/decisions/ADR-023-realtime-voice-carrier-with-body-delegation.md)
for the credential, projection and adapter contracts. Removing the old logger
does not enable this surface or establish carrier, installed-service or
physical-device acceptance.
