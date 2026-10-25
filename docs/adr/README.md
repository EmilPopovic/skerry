# Architecture decision records

One decision per file. **ADRs are immutable**: they record the context and reasoning at the
moment a choice was made. A decision that changes is not edited — a new ADR supersedes it
and links back, so the reasoning trail survives.

Format: Context · Decision · Consequences · Revisit trigger · Status.

| # | Decision | Status |
| --- | --- | --- |
| [0001](0001-custom-gpu-ui.md) | A custom GPU UI layer instead of GTK or Qt | Accepted |
| [0002](0002-own-platform-layer.md) | Own the Wayland/X11 platform layer instead of using `winit` | Accepted — [Spike D](https://github.com/EmilPopovic/skerry/blob/spikes-final/spikes/d-platform-and-dnd/README.md) resolved its revisit trigger |
| [0003](0003-wgpu-first-backend.md) | `wgpu` as the first GPU backend, behind a `Renderer` seam | Accepted |
| [0004](0004-swash-for-text.md) | `swash` for shaping and rasterization; our own line layout | Accepted |
| [0005](0005-no-async-runtime.md) | One owned job pool; no async runtime in the core | Accepted |
| [0006](0006-out-of-process-decoders.md) | Decoders in sandboxed helper processes | Accepted |
| [0007](0007-runtime-capability-detection.md) | Runtime capability probing, never build-time features | Accepted |
| [0008](0008-persistent-index-is-required.md) | A persistent filename index in `sk-index`, not an optional extra | Accepted |
| [0009](0009-x11-backend-behind-a-runtime-selected-enum.md) | An X11 backend behind a runtime-selected enum, with scale from `Xft.dpi` | Accepted |
| [0010](0010-a-split-tree-of-tab-strips.md) | A binary split tree whose leaves are tab strips | Accepted |
| [0011](0011-one-plain-text-state-file-written-atomically.md) | One plain-text state file, written whole and renamed into place | Accepted |
| [0012](0012-a-flat-index-file-per-root-rebuilt-whole.md) | One flat index file per root, rebuilt whole, verified at the point of use | Accepted |
| [0013](0013-one-cell-geometry-behind-every-uniform-view.md) | One cell geometry behind every uniform view, and Tree is not one of them | Accepted |
| [0014](0014-a-column-view-is-a-trail-of-panes.md) | A column view is a trail of panes, and only the last one is the pane | **Superseded by [0015](0015-a-column-is-a-directory-and-a-choice.md)** |
| [0015](0015-a-column-is-a-directory-and-a-choice.md) | A column is a directory and a choice, not a pane | Accepted |
| [0016](0016-plural-forms-are-bracketed-keys-in-the-catalogue.md) | Plural forms are bracketed keys in the catalogue | Accepted |
| [0017](0017-one-preview-child-per-kind-answering-one-request-at-a-time.md) | One preview child per kind, answering one request at a time | **Superseded by [0019](0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md)** |
| [0018](0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md) | Thumbnails are slots in one atlas; a preview is not | **Superseded by [0020](0020-a-preview-is-a-few-textures-bounded-by-bytes.md)** |
| [0019](0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md) | A preview request names a page, and the child stays stateless | Accepted |
| [0020](0020-a-preview-is-a-few-textures-bounded-by-bytes.md) | A preview is a few textures, bounded by bytes, and the pages ahead are rendered early | Accepted |
| [0021](0021-a-pin-is-a-line-in-gtks-bookmarks-file.md) | A pin is a line in GTK's bookmarks file, and rule 6 state rather than rule 3 state | Accepted |
| [0022](0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md) | A search names a scope, and everywhere is the local mounts, index-first per root | Accepted |
| [0023](0023-d-bus-is-a-socket-we-speak-ourselves.md) | D-Bus is a socket we speak ourselves: a thread to serve, a connection per call | Accepted |
| [0024](0024-a-page-arrives-with-its-words.md) | A page arrives with its words: the renderer's own text layer, in word boxes, beside the pixels | Accepted |
| [0025](0025-the-config-file-is-the-keymap-written-out.md) | The config file is the keymap written out, and the program never writes it behind you | Accepted |
| [0026](0026-at-spi-is-a-tree-we-publish-on-the-bus-we-already-speak.md) | AT-SPI is a tree derived from the frame, published on the bus we already speak | Accepted |
| [0027](0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md) | A setting is a line in the config; the session remembers what the window changed | Accepted |
| [0028](0028-the-page-you-are-on-and-its-two-neighbours-are-never-evicted.md) | The page you are on and its two neighbours are never evicted | Accepted |
| [0029](0029-markdown-renders-from-our-own-parser-and-unknown-syntax-is-its-own-text.md) | Markdown renders from our own parser, and unknown syntax is its own text | Accepted |
| [0030](0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md) | A volume is a device udev described, and mounting it is one udisks2 call | Accepted; **superseded on which devices are volumes by [0031](0031-a-partition-the-table-says-not-to-automount-is-not-a-volume.md)** |
| [0031](0031-a-partition-the-table-says-not-to-automount-is-not-a-volume.md) | A partition udev has flagged to be ignored is not a volume | Accepted |
| [0032](0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md) | An archive is unpacked by a child that can only write where the files go | Accepted |
| [0033](0033-operation-failures-retain-the-work-that-already-happened.md) | Operation failures retain the work that already happened | Accepted |
| [0034](0034-helper-execution-and-network-rights-are-explicit.md) | Helper execution and network rights are explicit | Accepted |
| [0035](0035-trash-recovery-uses-filesystem-local-records.md) | Trash recovery uses filesystem-local records | Accepted |
| [0036](0036-operation-failures-retain-pending-work.md) | Operation failures retain pending work | Accepted |
| [0037](0037-runtime-degradation-is-visible-and-injectable.md) | Runtime degradation is visible and injectable | Accepted |
| [0038](0038-independent-windows-own-separate-session-slots.md) | Independent windows own separate session slots | Accepted |
| [0039](0039-file-clipboard-intent-belongs-to-one-offer.md) | File clipboard intent belongs to one offer | Accepted |
| [0040](0040-bounded-parser-mutation-campaigns.md) | Bounded parser mutation campaigns | Accepted |
| [0041](0041-panic-diagnostics-use-a-preopened-local-log.md) | Panic diagnostics use a preopened local log | Accepted |
| [0042](0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md) | Where a mark comes from is a setting, and an icon is a band in its own atlas | Accepted |
| [0043](0043-recents-are-two-stores-read-as-one-list-and-ours-is-the-one-rule-6-counts.md) | Recents are two stores read as one list, and ours is the one rule 6 counts | Accepted — the shared write is staged behind the measurement the record names |
| [0044](0044-a-launch-that-names-nothing-is-a-new-window.md) | A launch that names nothing is a new window | Accepted — supersedes one sentence of 0038 |

## Triggers that have been answered

**0002's** was: *if Spike D demonstrates that a general-purpose windowing crate
can act as a drag-and-drop source against GTK and Qt on both display servers and
handle `text-input-v3` adequately, take it and delete this decision.* Spike D
measured `winit` reaching five of the nine capabilities
[05-platform-integration.md](../05-platform-integration.md) requires; the four it
misses are drag-source, primary selection, `text-input-v3` and exact fractional
scale. The trigger did not fire and the decision stands.

Two things since have been the decision earning itself rather than costing:
GNOME advertises no `zxdg_decoration_manager_v1`, so the window draws its own
frame — which is a thing to *have* implemented rather than to hope for from a
crate — and the compositor's own GPU is read from `zwp_linux_dmabuf_v1`, which is
a question a windowing crate does not know it is being asked.
