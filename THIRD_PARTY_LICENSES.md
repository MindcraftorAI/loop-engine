# Third-Party Licenses

`loop-daemon` includes code cherry-picked from upstream MIT-licensed
projects. Each lifted file carries an SPDX header noting the source and
upstream copyright. The full license text of each upstream project is
preserved below as required by MIT terms.

## affaan-m/everything-claude-code (ecc2/)

**Repository:** https://github.com/affaan-m/everything-claude-code
**Pinned commit:** `9a5ed3223aac8b927e5d4a17b6c7c0690eac0b44` (as of 2026-05-13)
**License:** MIT

Files lifted from this project:

| Source path (upstream) | Target path (here) | Lift kind |
|---|---|---|
| `ecc2/src/session/output.rs` | `src/buffer.rs` | Verbatim, with rename `SessionOutputStore` → `SessionRingBuffer` |
| `ecc2/src/session/daemon.rs` (lines 476-496) | `src/pid.rs` (the `pid_is_alive` helper) | Verbatim helper extraction |

Pattern adaptations (no code copied, but structure/approach is recognizably
derived):

| Source path (upstream) | Target path (here) |
|---|---|
| `ecc2/src/main.rs` (lines 1309-1322) | `src/main.rs` (tokio + tracing + clap entrypoint shape) |
| `ecc2/src/config/mod.rs` (lines 493-607) | `src/config.rs` (layered default → global → project merge pattern) |

### Upstream copyright notice (preserved verbatim per MIT terms)

```
MIT License

Copyright (c) 2026 Affaan Mustafa

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
