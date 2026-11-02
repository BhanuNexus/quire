# Model catalog

The models Quire can download, compiled into the app from
`crates/quire-ai/src/catalog.toml`. Owner: WS-D. Types: `quire_ai::catalog`.

```toml
format = 1
[[model]]
id = "bge-small-en-v1.5"
purpose = "embed"            # engine | embed | llm | ocr
label = "English"
licence = "MIT"
ram_mb = 250
dims = 384
[[model.files]]
name = "model.safetensors"
url = "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/<revision>/model.safetensors"
sha256 = "<64 hex>"
size = 133466304
```

Downloads go to `<data_dir>/models/<id>/<name>` and are checked against
`sha256`. Keys Quire doesn't know are kept on each entry. A catalog with a
newer `format` is refused. The catalog is empty until WS-D pins the models.
