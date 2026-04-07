# Task
## Refactoring and modularizing
Lib.rs is getting gigantic and hard to maintain. We need to refactor it into smaller, more manageable modules. This will improve readability and make it easier to navigate the codebase.
If the proc-macro code causes this largeness, concidder splitting it into a few separate rs files: one generates api struct part of the proc macro, anotherone generates endpoints, third one builders. That is just an example, you can split it in any way you see fit. The main goal is to reduce the size of lib.rs and improve code organization.

## Cache checking
Unless compiled with release, the app should be able to check if the cache is up to date with the upstream. Add function (structname).check_cache() that takes the specs and cache, compares 2 instances, prints the differences  intelligently (added this endpoint, remove this string from the function), updates the cache with the specs, and exits if the cache was updated just now. After that, the developer reads the differences and update the code accordingly.
In release mode, this function should do nothing and just return true. This will help us ensure that our cache is always in sync with the upstream specifications during development, while avoiding unnecessary overhead in production.
Test this with one of examples. Corrupt the cache, or make fake endpoints in the specs, and see if the function correctly identifies the differences, updates the cache and exits.
If possible, make a y/n to write the differences to a text file and exit or not. If y, write and exit. If n, don't write and proceed with the execution. This will give developers the option to keep a record of the differences for future reference.

## example sound_generator doesn't work
Identify the cause. Check the generator using python and gradio client. If it works, than the issue is likely in the [gradio_rs repo](https://github.com/jacoblincool/gradio_rs).
- Clone this repo on your environment.
- Try running this endpoint using this gradio_rs crate (gradio on crates.io) without macros. If it works, then the issue is likely in the macros.
- Try to fix the code in the gradio_rs repo. Search for new gradio api specs, and if you get it working, write an MD file in the main repo with the steps you took to fix it, i will open a PR to the gradio_rs repo and make gradio 6 implementation.

