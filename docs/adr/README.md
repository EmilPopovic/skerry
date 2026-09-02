# Architecture decision records

One decision per file. **ADRs are immutable**: they record the context and reasoning at the
moment a choice was made. A decision that changes is not edited — a new ADR supersedes it
and links back, so the reasoning trail survives.

Format: Context · Decision · Consequences · Revisit trigger · Status.

| # | Decision | Status |
| --- | --- | --- |
| [0001](0001-custom-gpu-ui.md) | A custom GPU UI layer instead of GTK or Qt | Accepted |
| [0002](0002-own-platform-layer.md) | Own the Wayland/X11 platform layer instead of using `winit` | Accepted — [Spike D](../../spikes/d-platform-and-dnd/README.md) resolved its revisit trigger |
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
