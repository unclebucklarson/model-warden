# The modelwarden Tutorial — plan

*Status: approved and executed — the tutorial lives at
[tutorial.md](tutorial.md). Decisions (2026-08-29): copy-paste sandbox,
single file, chapter 11 kept as optional-online, text-only for now —
GUI screenshots to be added later with a guided shot list. This plan is
retained as the curriculum's design record.*

## Goals and audience

Teach someone who runs local LLMs — but has never thought hard about
*storage* — everything they need to use warden confidently: every term,
every concept, every command, and crucially **why each feature exists**.
The existing User's Guide is a *reference* (organized by feature); the
tutorial is a *course* (organized by learning, with your hands on the
keyboard the whole way).

Pedagogy, one rule per chapter:

- **Concept → why it matters → do it → see it → check yourself.** Every
  chapter ends with an exercise the reader performs and a "you should now
  see…" checkpoint, plus 2–3 self-test questions.
- **Terms are defined at first use, bolded, and collected** in a
  final glossary. No term is used before it's taught.
- **Nothing in the tutorial touches the reader's real models.** All
  exercises run in a sandbox (below). The final chapter graduates the
  reader to their real stores.

## The sandbox (the tutorial's foundation)

Chapter 1 has the reader build a throwaway warden world — which is
itself the first lesson, because it teaches where warden keeps
everything:

```
mkdir -p ~/warden-tutorial/{config/modelwarden,state,shelf}
# config.json pointing at the sandbox shelf, discover_stores off
alias wt='XDG_CONFIG_HOME=~/warden-tutorial/config XDG_STATE_HOME=~/warden-tutorial/state warden'
```

Fake models are made with a provided one-liner that writes a real GGUF
header (magic + version) plus filler — small enough to hash instantly,
real enough that every warden feature works on them. This is exactly the
isolated-environment technique warden's own test suite uses; the reader
learns a legitimately useful skill (running warden against any directory
tree) as a side effect.

**Open question for review:** ship the sandbox as a copy-paste block
per platform, or as a small `docs/tutorial/setup.sh`? (Recommend:
copy-paste blocks — no curl-pipe-bash culture, and typing it teaches it.)

## Chapter outline

Each chapter: ~10 minutes, concept + exercise + checkpoint + self-test.

**1. The problem you already have** — why model storage sprawls: stores
owned by different tools with different lifecycle rules; the three real
incidents that motivated warden (silent cache pruning, moved refs,
invisible duplicates). *Terms: model file, GGUF, safetensors,
quantization, store, cache, shelf.* Exercise: build the sandbox, create
three fake models.

**2. Seeing what you have** — `scan` (live view) vs `hash` (the
catalog); reading `status`. *Terms: catalog, scan, fingerprint, SHA-256,
content identity, manifest, inventory.* Exercise: scan, hash, run hash
again and notice nothing re-hashes — then touch a file and watch only it
re-hash. Teaches the two-tier identity by direct observation.

**3. Same bytes, different names** — the duplicate problem. *Terms:
duplicate, hardlink, inode, reclaimable.* Exercise: copy a model under a
new name, watch `dups` find it, `dedup` dry-run, then `--hardlink`; use
`ls -i` to see two names share one inode. The "same bytes" idea becomes
physical.

**4. Drives and the meaning of "offline"** — registered roots, labels,
durable identity. *Terms: root, registered root, label, marker file,
offline-not-gone.* Exercise: register a folder as a pretend drive with a
label, back up to it, then `mv` it away and watch `where` say OFFLINE —
then bring it back. The catalog's memory becomes visible.

**5. Backups you can actually trust** — verified copies; the three-way
hash check. *Terms: verified copy, .partial, refuse-overwrite,
backup coverage.* Exercise: selective backup by name; read the "N of M
backed up" headline before and after.

**6. Bit rot, and catching it (the showpiece)** — why bytes decay and
why backups are most at risk. *Terms: bit rot, scrub, verify, repair.*
Exercise: flip one byte in the backup copy with `dd`, watch `verify`
fail loudly, run `--repair`, watch it heal from the live source, verify
green. The single most convincing five minutes in the tutorial.

**7. Bundles: models are more than one file** — split parts,
projectors, safetensors directories. *Terms: bundle, split model,
mmproj/vision projector, companion, container.* Exercise: create a fake
split pair + projector; back up "one" model and count what traveled.

**8. Cold storage** — the demote/restore lifecycle; Active vs All.
*Terms: cold storage, demote, verified move, restore, promote.*
Exercise: bulk-demote two models to the pretend drive with
`--remove-source`, observe the shelf empty but the catalog complete,
restore one.

**9. Deleting without fear** — the two-stage trash. *Terms: trash,
restore, empty, irreversible.* Exercise: delete a split model, inspect
`trash`, restore it (watch the whole bundle return), delete again,
`empty --yes` — the reader performs warden's only irreversible act
knowingly.

**10. The doctor and the journal** — health findings with explanations
and loss statements; the operations journal as memory. *Terms: finding,
remedy, owner command, husk, orphan blob, advisory, journal.* Exercise:
read `journal --all` and recognize every chapter they just performed —
the tutorial literally ends up written in the journal.

**11. Getting models (online, optional)** — fetch by pattern, split
sets, projector auto-include, snapshots, tokens and the 401 story.
*Terms: repo, revision, provenance, gated repo, token, snapshot.*
Exercise: fetch a genuinely tiny real model (prajjwal1/bert-tiny,
~17 MB), then `where` it and see provenance. Clearly marked optional
(needs network).

**12. Graduation: your real machine** — leave the sandbox; the
first-run sequence on real stores; what to expect from a first `doctor`
on a lived-in cache; the scrub timer; reading `status` as a habit.
Ends with the safety model recap: the five rules and the one
irreversible command.

**Appendices:** A. full glossary (every bolded term, one place);
B. command ↔ chapter cross-reference; C. "when things look wrong"
(pointing into the User's Guide FAQ rather than duplicating it).

## Format and placement

- **Recommend: one file, `docs/tutorial.md`**, chapters as `##`
  headings with a linked table of contents. Single-file keeps GitHub
  reading/searching simple and matches the guide's convention. (Alternative:
  `docs/tutorial/NN-name.md` per chapter — better if we later adopt
  mdBook/a docs site; easy to split then.)
- README gets a second callout: *"New to warden? The Tutorial teaches
  it hands-on in a sandbox; the User's Guide is the reference."*
- The User's Guide stays the reference; the tutorial links into it
  rather than duplicating tables (findings glossary etc.).

## Quality bar

- Every command block is **executed against a real sandbox during
  writing** (same discipline as the QA doc) — no untested snippets.
- Checkpoints state exact expected output shapes so a reader knows
  immediately when something's off.
- Estimated length: ~1,000–1,300 lines; a motivated reader finishes in
   90–120 minutes.

## Open questions for review

1. Sandbox as copy-paste blocks (recommended) or a setup script?
2. Chapter 11 online exercise: keep (real fetch, tiny model) or make it
   read-along only?
3. Single-file vs per-chapter files (recommended: single file for now)?
4. Screenshots: the GUI appears throughout as "the same operation in the
   GUI" sidebars — text-only for now, or wait for your screenshot pass?
