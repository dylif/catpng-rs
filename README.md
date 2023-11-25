# catpng-rs
A tool for concatenating PNGs.
## Building/Running
To build the project, run `cargo build`. To build and run the project, run
`cargo run ARGS`, replacing `ARGS` with your desired arguments. To build and run
tests, run `cargo test`.
## Usage
`OUTPUT LEVEL INPUT...`
 - `OUTPUT`: The output file path
 - `LEVEL`: The output file compression level (0-10, default: 10)
 - `INPUT`: The input file(s)

## Limitations
Currently, only PNGs with common IHDRs (except height) and one IDAT chunk each
can be concatenated.