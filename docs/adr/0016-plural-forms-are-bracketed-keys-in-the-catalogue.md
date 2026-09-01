# 0016 — Plural forms are bracketed keys in the catalogue

**Status:** Accepted.

## Context

A folder holding one file says **`1 items`**. So does the Properties panel, the
search summary, the indexing line, and the status line's filtered and collapsed
forms — twelve strings in
[en.txt](../../crates/sk-model/catalogue/en.txt), each written with one noun and no
way to agree with its count.

[`sk_model::i18n`](../../crates/sk-model/src/i18n.rs) is a hundred lines instead of
a localization framework, for the reason
[07-dependencies.md](../07-dependencies.md) gives: the framework is a dependency
and a build step, and this is a lookup. It resolves a key to a line and
interpolates `{0}`, `{1}` positionally so a translation can reorder clauses. It has
no concept of a count.

**The pattern has already been hand-rolled three times**, which is what makes this
a decision rather than a feature request:

| Site | Shape |
| --- | --- |
| `view.rs` — the permanent-delete panel | `erase.title_one` / `erase.title_many`, branched at the call site |
| `app.rs` — the copy notice | `notice.copied_one` / `notice.copied` |
| `app.rs` — the cut notice | `notice.cut_one` / `notice.cut` |

Reading those three closely is what settles the shape of the mechanism, because
**none of them is a plural**. Each takes *different arguments* in its two branches:
one file is named — `Delete "report.pdf" for good?` — and several are counted —
`Delete 4 items for good?`. A message that says a different thing about one item is
a different message. What is missing is the narrower case: **the same sentence with
the same arguments, where only agreement changes.**

Three things constrain the answer.

**The catalogue is the expensive part to change.** The lookup function can be
rewritten in an afternoon; a file format cannot, once translators have files in it.
This decision is about the format and only incidentally about the code.

**English needs two forms and the format has to admit six.** CLDR's categories are
`zero one two few many other`; Polish uses four, Arabic six. A format that can only
express one/other would have to be replaced by the first translation that needs
`few`, which is the first Slavic one.

**Suffixes are already taken.** `status.counts.none` means *nothing is selected*,
not *zero items*; `erase.title_one` means *one file, named*. A plural scheme that
appended `.one` and `.other` would share a namespace with meanings that are already
in it, and `status.counts.none.one` is a key nobody can read.

## Decision

**A plural key carries its category in brackets, and the catalogue declares which
rule picks between them.**

```
plural.rule = one-other

status.counts[one]   = {0} item, {1} selected
status.counts[other] = {0} items, {1} selected
```

- `i18n::plural(key, count, args)` resolves `key[category]`, where the category
  comes from `count` through the rule the catalogue names. It falls back to
  `key[other]`, then to `key` itself, so an incomplete translation degrades to a
  readable line rather than to a key on screen.
- **Brackets, because they cannot collide.** A key is `[a-z0-9_.]` today, so
  `status.counts` and `status.counts[one]` are distinct strings and no existing
  suffix — `.none`, `_one`, `_many` — is disturbed.
- **The count is an argument, not `args[0]`.** Which number governs agreement is
  per string: `{0} items` agrees with the first, and `{0} of {1} items match`
  agrees with the *second*. A mechanism that inferred it would be right by
  coincidence in half the catalogue.
- **One rule is implemented: `one-other`.** The seam is the `plural.rule` line, not
  a table of every language's arithmetic — unused arms for languages with no
  catalogue would be code with no test. A rule name this build does not implement
  is a **test failure**, not a silent fall back to `other`, because falling back
  would put `1 items` back on screen and call it localization.
- **The three hand-rolled pairs stay as they are.** They are two messages each, and
  folding them into a plural would mean a form whose `{0}` is a filename in one
  category and a count in the other.

## Consequences

Twelve strings gain a second line and a bracket, and one function joins the
module. `t!()` grows a plural form so a call site reads
`tp!("properties.items", n, n)` — the count twice, once to choose and once to
interpolate, which is honest about the two jobs it does.

A translator adds forms by adding lines, and declares their language's rule once at
the top of their file. Nothing about an existing key changes, so a file written
against the old format keeps working — every key without brackets is looked up
exactly as before.

**The gate that already exists gets stricter for free.**
`tools/check-invariants.py` fails a build on a key a source file asks for and the
catalogue does not have; `plural` asks for `key[one]`, so a missing form is a
build failure at the call site that needs it — for the ones written as literals.
The pattern this file has hit twice recently applies: a key assembled at run time
is invisible to a regex, so the catalogue's own test asserts that every plural key
has an `[other]` form and that no bracketed key is orphaned.

The cost of not doing this now is the one that grows: the moment a second catalogue
exists, adding plurals means asking a translator to revisit every counted string.

## Revisit trigger

A language whose rule cannot be named by a single token per catalogue — a rule that
depends on the *formatted* number rather than the value, which CLDR expresses with
`i`, `v` and `f` operands. English ordinals and Welsh are the usual examples. At
that point the rule line becomes a rule *expression* and this decision is
superseded rather than edited.

A second trigger, from the other direction: if the count of plural strings stays
near twelve and no second catalogue arrives within a milestone, the seam is
speculative and the honest move is to say so here in the ADR that supersedes this
one.
