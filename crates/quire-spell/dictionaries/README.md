# Bundled spelling dictionaries

| Files | Language | Source |
|---|---|---|
| `en_US.aff`, `en_US.dic.gz` | English (United States) | npm [`dictionary-en`](https://www.npmjs.com/package/dictionary-en) 4.0.0 |
| `en_GB.aff`, `en_GB.dic.gz` | English (United Kingdom, "-ise" spellings) | npm [`dictionary-en-gb`](https://www.npmjs.com/package/dictionary-en-gb) 3.0.0 |

Both packages come from [wooorm/dictionaries](https://github.com/wooorm/dictionaries),
which repackages the Hunspell dictionaries generated from
[SCOWL](http://wordlist.aspell.net/) by Kevin Atkinson, version 2020.12.07
(SCOWL size 60, accents stripped; see the header of each `.LICENSE` file).

**License.** The word lists and affix files are under the SCOWL / Ispell /
WordNet / UKACD terms reproduced verbatim in `en_US.LICENSE` and
`en_GB.LICENSE`: permissive (MIT-like and BSD-like) notices plus public-domain
sources. They must ship with any copy of Quire, for example in the app's
third-party notices.

**Changes.** None to the content. The npm files `index.aff`, `index.dic` and
`license` were renamed, and the `.dic` files were compressed with
`gzip -9 -n` so the binary carries ~190 KB per language instead of ~550 KB.
To check or reproduce:

```sh
npm pack dictionary-en@4.0.0 && tar xzf dictionary-en-4.0.0.tgz
sha256sum package/index.aff package/index.dic
# 8ae1f19d4840d957728ad90555d5a8dff6cc5c046279c95ff0c00fc0a0136c7b  index.aff
# f0b1a234bd178bdd01875b2a392a9647f888b8fe879f79c52aae62c2759b3647  index.dic
zcat en_US.dic.gz | sha256sum   # same as index.dic
```

`dictionary-en-gb@3.0.0`: `index.aff` is identical to the US one;
`index.dic` sha256 is `869fe17ba4ee4b5401c60a666ee2d6a3dcc237f460b7294435df1cc6a799aa57`.
