# v0.1.0

## Changelog since v0.1.0

### Known Issues

No new known issue.

### Security Notes

No new security note.

### Changes

#### Feature

- add program `typst-ts-cli`, with [commands: compile, font:list](https://github.com/Myriad-Dreamin/typst.ts/blob/2478df888282af09dc814a481348745c4311f98f/cli/src/lib.rs).

- add program `typst-ts-fontctl` to download font assets from typst repo, [ref](https://github.com/Myriad-Dreamin/typst.ts/blob/2478df888282af09dc814a481348745c4311f98f/contrib/fontctl/src/main.rs).

- add `typst_ts_core::Artifact` to represent a precompiled document.

- add `typst_ts_core::Artifact::to_document` to convert an artifact to a `typst::doc::Document`.

- introduce `typst_ts_core::config::WorkspaceConfig` for configure workspace for compiler.

- introduce `typst_ts_core::config::CompileOpts` for control low-level behavior of compiler.

- add `@myriaddreamin/typst.ts/createTypstRenderer(pdfjsModule): TypstRenderer`.

- add method `init` and method `render` to `@myriaddreamin/typst.ts/TypstRenderer`.

- add `@myriaddreamin/typst.ts/preloadRemoteFonts: BeforeBuildFn`.

- add `@myriaddreamin/typst.ts/preloadSystemFonts: BeforeBuildFn`.

- add `@myriaddreamin/typst.react/<TypstDocument fill?='' artifact=''>`.