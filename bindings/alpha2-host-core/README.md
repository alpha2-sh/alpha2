# alpha2-host-core

Native host logic without FFmpeg. Every native host shares its policies, audio timelines and
output, cursor rules, providers, extraction, plugin runtime, protocol frames and, with the
`requests` feature, the desktop host's request parsing. The mobile hosts also run its decode,
export and still pipeline over their platform's decoders and encoders; the desktop host decodes
and encodes through FFmpeg instead.

Part of [alpha2](https://github.com/alpha2-sh/alpha2), a media engine that renders timeline projects on the GPU.
