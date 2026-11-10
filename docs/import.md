# Importing notes

**File → Import…** (or `quire-cli import`) reads an export from another app,
shows what would happen, and only then writes Markdown notes into your vault.
Nothing is written until you confirm, nothing existing is overwritten, and the
whole import can be undone.

## The wizard

1. **Source.** Pick what you are importing from (or "Find out for me") and the
   file or folder.
2. **Preview.** A dry run, read in the background: counts of notes and files,
   warnings (links that point nowhere, things that couldn't be converted),
   notes whose names are already taken in the target folder, and a few
   converted notes to read before you decide.
3. **Folder.** Where the notes go. The default is a new folder, `Imported`, so
   the import is one place you can look through, move or delete.
4. **Import.** Written in the background with progress and Cancel. A
   cancelled import keeps what was written, and that can be undone.
5. **Report.** What was written, what was renamed because the name was taken,
   what was refused as unsafe, and **Undo import**.

Undo is also **Undo the last import** in the command palette and
`quire-cli import <vault> --undo last`.

## Sources

| Source | Give it | What comes across |
|---|---|---|
| Logseq | the graph folder | pages and journals; `[[links]]`, tags and properties; block references are resolved; assets |
| Notion | the Markdown & CSV `.zip`, or its folder | pages, database CSVs as notes with properties; links between pages rewritten as wikilinks; the 32-character ids Notion adds to names are removed |
| Bear | a `.bear2bk` backup or a folder of TextBundles | notes, `#tags`, attached images and files, created and modified dates |
| Evernote | an `.enex` file | notes, tags, created and updated dates, attachments (images, PDFs); ENML converted to Markdown |
| Roam Research | `Export all → JSON` | pages, nested blocks as lists, `[[links]]`, `#tags`, block references, daily notes named by date |
| Google Keep | the Keep folder of a Google Takeout (or its `.zip`) | notes, checklists as tasks, labels as tags, pinned, archived and colour in front matter (trashed notes are left out), created and edited times, images and audio |
| Joplin | a `.jex` file or a RAW export folder | notes in their notebooks, tags, resources, `:/id` links rewritten as wikilinks, created and updated times |
| Day One | the `.zip` or `Journal.json` from `Export → JSON` | entries as notes named by date, tags, photos and other media, location, weather, starred |
| HTML | a folder of `.html` files | converted to Markdown with headings, lists, tables, links and images; covers exports of Apple Notes, OneNote and saved pages |
| Markdown | a folder of `.md` and `.txt` files | copied as they are; the folder structure is kept |

In every source Quire keeps **attachments** (copied into the vault's
attachments folder, links to them rewritten), **dates** (a note's created and
modified times are set on the file and written as properties when the source
has them), **tags** (as `#tags` or `tags:`), and **internal links** (rewritten
as `[[wikilinks]]` when the target is in the same export; a link to something
that isn't in the export stays as it was and is counted in the warnings).

Text in other encodings (UTF-16, Latin-1 and the like) is detected and
converted to UTF-8.

## Safe on hostile archives

An export is untrusted input. Loading it follows these rules, and each is
tested with a fixture that breaks it:

- A path that leaves the export or the target folder (`../`, absolute paths,
  drive letters, backslash tricks) is refused and listed in the report.
- Symbolic links, hidden folders (`.git`, `.quire`, `.trash`), and Windows
  device names are skipped.
- Limits (the defaults are 100,000 files, 2 GiB in all, 256 MiB in a file,
  32 levels deep, 255 bytes in a name) stop a flood. Reaching one skips the
  rest and says so.
- A file hundreds of times larger than its compressed size is a zip bomb and
  is skipped.
- Nothing in an export is ever run, and HTML is converted, not rendered.
- Writing never overwrites: every file is created only if its name is free. A
  taken note name becomes `Name 1.md`; an attachment whose name a different
  file has gets a new name and the imported notes' links follow; an identical
  attachment is not copied again.
- An attachment named by many notes is copied once per file in the export.
- Importing into a locked folder works while it is unlocked (notes and
  attachments are sealed, never written in plain) and is refused, before
  anything is written, while it is locked.

## Undo

Each import writes a record to `.quire/imports/` with the hash of every file
it created ([format](formats/import-record.md)). **Undo** moves the files that
are still exactly as the import wrote them to `.trash/`. A file you edited
since stays where it is and is listed. Nothing outside the record is touched,
so notes you had before the import are never at risk. Each imported note also
has a first history version marked "Imported (Source)".

## From the command line

```bash
quire-cli import ~/Downloads/export.jex ./vault                 # dry run
quire-cli import ~/Downloads/export.jex ./vault --from joplin --into Imported --write
quire-cli import --list ./vault                                  # past imports
quire-cli import --undo last ./vault
```

## Not supported yet

Apple Notes directly (its database isn't a documented format; export to HTML
or use the Notes app's own export), OneNote `.docx`/`.one`, CSV and Airtable
tables, Tomboy, and Craft (its Markdown export is read as Markdown). Run a dry
run on your real export first: the importers are tested on fixtures that
follow each app's documented format.
