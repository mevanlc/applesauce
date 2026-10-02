# Applesauce

Applesauce is a command-line interface (CLI) program written in Rust that
compresses, decompresses, and prints information about compressed files for
HFS+/APFS transparent compression on macOS. It is based on
[afsctool](https://github.com/RJVB/afsctool) and offers several key
improvements, including better performance, improved multithreading (even for a
single file), and reduced memory usage. Applesauce supports all three compression
algorithms used by HFS+/APFS: LZFSE, LZVN, and ZLIB.

![compression example](https://github.com/user-attachments/assets/3f78a011-7db4-4e7f-9cc9-89781ed78609)

## Installation

### Install via Homebrew

To install Applesauce using Homebrew, run the following command:

```console
brew install Dr-Emann/homebrew-tap/applesauce
```

### Install prebuilt binaries via shell script

```console
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Dr-Emann/applesauce/releases/latest/download/applesauce-cli-installer.sh | sh
```

### Manual build with Cargo

<details>
<summary>Details</summary>

To install Applesauce using Cargo, follow these steps:

1. Install Rust and Cargo using the instructions provided at [rust-lang.org](https://www.rust-lang.org/tools/install).
2. Clone this repository to your local machine.
3. In the project directory, run `cargo build --release` to build the program.
4. The built binary can be found in the `target/release` directory.

</details>

## Usage

To use Applesauce, run the following command:

```console
applesauce [compress|decompress|info] [OPTIONS] <PATHS>...
```

The options are as follows:

- `compress`: Compresses the specified file/directory using one of three compression algorithms (LZFSE, LZVN, or ZLIB).
- `decompress`: Decompresses the specified file/directory.
- `info`: Prints information about each specified file/directory, including the compression ratio and
  compression algorithm used. A progress bar tracks files as they are inspected. Pass `--summary` to print one
  aggregate file and storage summary across all paths using the same file and storage accounting as compression.

For example, to compress a file named `example.txt` using the ZLIB compression algorithm, you would run:

```console
applesauce compress -c ZLIB example.txt
```

To print one aggregate summary for multiple paths, run:

```console
applesauce info --summary path/to/first path/to/second
```

Files that do not reach the compression threshold are skipped. Use `-v` /
`--verbose` to report these skips, including with either scratch mode.

### Scratch storage for slower destinations

Use `--scratch DIR` or `--scratch-memory` with `compress` or `decompress` to stage
output on a fast disk or in RAM before copying completed files to a slower destination:

```console
cd /Volumes/SLOWER
applesauce compress --scratch="$TMPDIR" .
applesauce compress --scratch="$TMPDIR" --scratch-limit=32GiB --verify .
applesauce compress --scratch="$TMPDIR" --scratch-batch .
applesauce compress --scratch="$TMPDIR" --scratch-batch=4GiB --verify .
applesauce compress --scratch-memory --scratch-limit=64GiB --scratch-batch .
applesauce decompress --scratch="$TMPDIR" .
applesauce decompress --scratch="$TMPDIR" --scratch-limit=32GiB --manual --verify .
applesauce decompress --scratch="$TMPDIR" --scratch-batch=4GiB .
applesauce decompress --scratch-memory --scratch-limit=64GiB --scratch-batch .
```

Compression workers stage encoded payloads; decompression workers stage ordinary
uncompressed data. Both use a private temporary directory under `DIR`.
One copy worker transfers completed payloads in sequential chunks of up to 4 MiB
to temporary files on the destination volume, then atomically
renames each completed file over its original. The scratch directory need not
support APFS compression. Destination files still require a filesystem that
supports transparent compression for compression operations. With `--verify`,
the destination temporary file is read back and compared with the original before
replacement. Decompression supports both the default OS decoder and `--manual`;
manual verification also decodes the original manually.

Use `--scratch-memory` to stage the same payloads in process memory instead of a
scratch directory. It is mutually exclusive with `--scratch DIR` and works with
`--scratch-limit`, `--scratch-batch[=SIZE]`, every compression backend, and both
decompression modes. Buffers allocate as output arrives and move directly to the
copy worker without being written to a scratch file. The copy worker writes the
encoded resource fork and compression metadata (or decompressed data) to a
destination-volume temporary file, verifies if requested, and atomically replaces
the original. Completed buffers are freed after publishing. Small compressed
files can store their entire payload in the compression xattr instead of a resource fork.

The default limit for disk or memory scratch is **16 GiB**. `--scratch-limit` accepts positive
integer byte counts, decimal units such as `16GB`, and binary units such as
`16GiB`. The limit covers active reservations and completed outputs, excluding
filesystem allocation/metadata overhead and destination temporary files. Memory
scratch counts allocated payload-buffer capacity and compression metadata, and
excludes worker buffers, allocator overhead, transient allocations during buffer
growth, and OS file caches. It is a staging budget, not a process-memory ceiling;
leave RAM available for those other uses. Process memory remains subject to the
OS's normal memory management, including swapping under memory pressure. Before
reading a file, Applesauce reserves its worst-case compressed size, including
format overhead, or its full uncompressed size for decompression. It waits when
the remaining budget is insufficient, then reduces the reservation to the stored
payload size (allocated buffer capacity for memory scratch) and metadata when
compression finishes. Space is released after copying and deleting or freeing
the scratch payload. A file whose full
reservation exceeds the limit is left unchanged with an error; increase the
limit to process it.

`--scratch-batch[=SIZE]` alternates staging and publishing whole-file batches.
Applesauce stops admitting source reads when the next file's reservation would
exceed the batch target, waits for all admitted files to finish staging, then
copies the completed batch back, deletes or frees its scratch payloads, and waits for a
flush of each affected destination volume before resuming source reads.
The flush covers the volume, including other applications' pending writes.
The final partial batch is also published. This works with both
compression and decompression, including `--manual`.

During publishing, an additional darker-green **Batch flush** progress bar shows
destination bytes copied versus the current batch's actual staged output size.
It counts encoded payloads and compression metadata for compression, or expanded
data for decompression. It is separate from the overall green bar and the white
per-file bars, which clear as their files finish publishing. After copying, it
displays **Flushing volume** until the volume
flush finishes, then clears and resets for the next batch. Byte progress measures
copies accepted by the filesystem; the volume flush provides the final disk wait.

Without a size, `--scratch-batch` uses the scratch limit as its target. An explicit
size requires `=`, as in `--scratch-batch=4GiB`, and must be positive and no greater
than the scratch limit. The target counts completed output and worst-case
reservations for files still being processed. A file whose reservation exceeds
the target gets its own batch if it fits the hard scratch limit. Staged files
have their handles closed until publishing, so large batches of small files do
not keep an open handle for every source and payload.

Scratch storage is removed on normal completion. Without `--scratch-batch`,
reads from the destination can still overlap with the copy worker's writes.
Batching separates bulk source reads from publishing, but verification and
filesystem metadata operations can still read during publishing. Filesystem
allocation and writeback determine physical disk access, so performance gains depend on the
device and workload. Without `--scratch` or `--scratch-memory`, Applesauce uses
its existing streaming pipeline.

## Features

Applesauce has the following key features:

- Supports three compression algorithms: LZFSE, LZVN, and ZLIB.
- Can print information about compressed files, including the compression ratio and compression algorithm used.
- Supports transparent compression for HFS+/APFS on macOS.

## Compression Algorithms

Applesauce supports three compression algorithms:

- LZFSE: This compression algorithm was developed by Apple for use on iOS and
  macOS. It is a fast compression algorithm that offers a good balance
  between compression ratio and speed.
- LZVN: This compression algorithm was also developed by Apple for use on iOS
  and macOS. It is optimized for use on 64-bit processors and offers a high
  compression ratio.
- ZLIB: This is a widely used compression algorithm that is implemented in many
  software packages. It is slower than LZFSE and LZVN, but can offer a higher
  compression ratio (especially with `-l 9`, but not always: it depends on the
  type of data being compressed).

Applesauce defaults to using LZFSE compression.
Depending on the type of data being compressed and the desired balance between
compression ratio and speed, one of these algorithms may be more suitable than
the others.

### LZFSE backends

`-c lzfse` selects the file format; `-b, --backend` selects its encoder:

| Backend | Implementation | Tuning |
| --- | --- | --- |
| `macos` | Apple's macOS compression library | Fixed |
| `crate` | Unmodified `lzfse-sys` 2.0.0, compiled and linked statically | Stock |
| `vendor` | Vendored Apple reference encoder, compiled and linked statically | Stock |
| `vendor-ultra` | Vendored Apple reference encoder, compiled and linked statically | Higher compression effort, using more time and memory |

All four are included in normal macOS builds. `--backend` is only valid with
LZFSE; omitting `-c` uses the default algorithm, LZFSE.
`-l` / `--level` only affects ZLIB, which accepts levels 1-12 and defaults to 5.
Explicitly specifying a level with any LZFSE backend or with LZVN emits a warning,
including when specifying `-l5`. Omitting the level emits no warning.
`-q` keeps this warning visible; `-qq` suppresses it along with the final summary.

```console
applesauce compress -c lzfse -b macos --verify path
applesauce compress -c lzfse -b crate --verify path
applesauce compress -c lzfse -b vendor --verify path
applesauce compress -c lzfse -b vendor-ultra --verify path
```

The `vendor-ultra` backend increases hash bits from 14 to 16, hash width from 4
to 8, and the good-match threshold from 40 to 100. It uses eight times as much
history-table memory and spends more effort looking for matches. Output may be
smaller, at a cost in encoding time and memory; higher effort does not guarantee
smaller output for every file. Both vendor backends have fixed settings unaffected
by `-l`. Inputs below 4096 bytes retain the reference encoder's LZVN fallback.
See the [vendor notes](crates/applesauce-core/vendor/lzfse/README.md) for provenance.

All backends write standard LZFSE files readable by macOS and Applesauce's manual
decoder. The file's compression metadata records LZFSE, not the backend or level.
Already-compressed files are skipped: decompress a copy first when comparing
backends. Streaming output, disk scratch, and memory scratch support every backend.

The default backend is `crate`. Build features select a different default while
keeping every backend available at runtime:

```console
cargo build --release -p applesauce-cli --features system-lzfse
cargo build --release -p applesauce-cli --features vendor-lzfse
cargo build --release -p applesauce-cli --features vendor-ultra-lzfse
```

`system-lzfse` selects `macos`; `vendor-lzfse` selects `vendor`;
`vendor-ultra-lzfse` selects `vendor-ultra`. When multiple default-selection
features are enabled, precedence is `vendor-ultra-lzfse`, then `vendor-lzfse`,
then `system-lzfse`. Explicit `-b` always overrides the build default.

## Improvements Over Afsctool

Applesauce is based on afsctool, but offers several key improvements, including:

#### Improved Performance

afcstool can compress multiple files in parallel, but applesauce parallelizes at
the block level, so even a single file can be compressed in parallel. Applesauce
can often be several times faster than afsctool, especially for small numbers of
large files.

#### Reduced Memory Usage

afcstool will load the entire file into memory before compressing it
(although it does attempt to use mmap for large files). By default, Applesauce
only keeps the blocks currently being processed in memory. Opting into
`--scratch-memory` also holds staged output within the configured scratch budget.

#### Better User Interface

Applesauce outputs a pretty progress bar while it's working, providing a more
user-friendly experience than afsctool's sparse output.

#### Better Compression With Many Small Files

afsctool will compress a file which fits in the xattr after compression, even
if doing so actually adds more overhead than leaving the file uncompressed.
Applesauce will not compress a file if it would result in a larger file.

#### Better Error Handling

afcstool overwrites files in place. Although it attempts to restore the file
if an error occurs, if it is forcefully terminated while compressing a file,
the file may be left in an invalid state.

Applesauce compresses/decompresses files to a temporary file and then atomically
renames the temporary file to the original file only when the operation is
complete: the file is never left in an invalid state, even if the program is
harshly terminated.

This is no replacement for backups: please do not use applesauce on files you
cannot afford to lose.

## License

Applesauce is licensed under the GNU General Public License version 3 (GPLv3).

## Contributions

Contributions to Applesauce are welcome! If you would like to contribute code,
please open a pull request on the GitHub repository. If you find a bug or have
a feature request, please open an issue on the repository.
