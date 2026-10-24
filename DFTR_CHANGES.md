# Dftr changes

Every change this fork carries on top of upstream, newest first: what changed, why, the test that
holds it, and whether upstream has it (AGENTS.md §12 in github.com/BashirAbu/RustyDftr).

Base: upstream release tag `v1.18.1`.

| Commit | What changed | Why | Test | Upstream has it |
|---|---|---|---|---|
| `65cd9fe` | Android: a key event types the character its key character map gives for the key and its modifiers (Shift+`;` types `:`, Shift+`a` types `A`); keys that type nothing printable keep the fixed table | The fixed table ignored the meta state, so a hardware keyboard, and `adb shell input text`, typed `ws;//` for `ws://` | Dftr's `platform_phone_…` scenario types a server address with colons on the phone | no |
