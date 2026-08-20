# Merged inventory — published schema (version 1)

`~/.local/state/modelwarden/inventory.json` is modelwarden's read-only
contract with consumers (llamacppCodeConf reads it to know what exists
without managing storage). From `schema_version: 1` on, shape changes bump
the version and get a note here. Consumers must ignore unknown fields.

Regenerated atomically (temp + rename) by every `warden hash` and by every
warden write operation. Read it, never write it.

```jsonc
{
  "schema_version": 1,
  "generated_unix": 1786905000,        // when this merge was produced
  "roots": [
    {
      "id": "shelf-0079bf06",          // stable; drives keep ids across remounts
      "kind": "shelf",                 // shelf | ollama | hf_hub | removable
      "path": "/home/buck/models",     // where it is (or was last) mounted
      "label": null                    // user label for drives
    }
  ],
  // Keyed by content identity:
  //   "sha256:<64 hex>"            — hashed (normal case)
  //   "pending:<dev>:<ino>:<size>" — seen, hash not computed yet
  //   "unknown:<root>:<relpath>"   — bytes unreachable (pruned/dangling)
  "models": {
    "sha256:bee238bb…": {
      "size": 17924717632,
      "display_name": "unsloth/Qwen3.8-27B-GGUF",
      "meta": {                        // GGUF header, null if unreadable
        "architecture": "qwen3",
        "name": "Qwen3.8 27B Instruct",
        "context_length": 262144,
        "quantization": "Q4_K_XL",
        "size_label": "27B"
      },
      "locations": [                   // every place these bytes live
        {
          "root_id": "shelf-0079bf06",
          "kind": "shelf",
          "rel_path": "Qwen3.8-27B-GGUF/Qwen3.8-27B-UD-Q4_K_XL.gguf",
          "accessible": true,          // as of generated_unix; also check
                                       // that the root's path exists NOW
          "dev": 66306,                // (dev, ino) tell hardlinks (same)
          "ino": 45370551              // from copies (different); 0 = unknown
        }
      ]
    }
  }
}
```

Semantics consumers must honor:

- **Identity is the key.** Two locations under one key are the same bytes.
  Never treat path or filename as identity.
- **`accessible: false` or a missing root path means offline, not gone.**
  Don't drop such entries; report them as on offline media.
- **Absolute path** of a location = `roots[id].path` + `/` + `rel_path`.
- **`pending:`/`unknown:` keys are transitional** — a later `warden hash`
  resolves them into `sha256:` keys.

Since M12 the catalog also holds non-GGUF model files (safetensors weights
and their tokenizer/config companions). Consumers must not assume every
entry is a `.gguf`; filter by file extension of `rel_path` if only GGUFs are
wanted. Shape and version are unchanged.

Per-root manifests (`~/.local/state/modelwarden/roots/*.json` and
`<drive>/.modelwarden/manifest.json`) share the version number but are
warden-internal; consumers should read only the merged inventory.
