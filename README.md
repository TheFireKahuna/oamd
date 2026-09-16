# oamd

Parser for Dolby Object Audio Metadata, the payload that says where the objects of an immersive
mix sit and how the bed folds down.

This crate currently provides the OAMD parser used by `truehdd/truehd`, and general-purpose use
across codecs is future work. Native AC-4 metadata has a different wire syntax and is not
currently supported by this parser.

```rust
let payload = oamd::ObjectAudioMetadataPayload::read(bytes)?;
```

Parsing only, and no rendering: the crate says what a payload states, not what a renderer should
do about it. Fields currently mix retained wire codes with decoded values: for example, bed
downmix gains retain codes, while object gain, priority, and ramp duration are converted by
the parser. Consult the individual fields before interpreting them.

Requires Rust 1.88 or later.
