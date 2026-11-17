//! The reference libjxl, linked statically: this crate has no code of its own, its build script
//! compiles the library (see `build.rs`) and the link directives come with it. A crate that calls
//! libjxl through `gamut-jxl` names this one (`use iv_libjxl as _;`) so it is linked.
