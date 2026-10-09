Pure runtime — play your own copy of Pure (Xbox 360, 2008) on this computer
============================================================================

This is an unofficial fan project. It contains NO part of the game: you need your own disc
image of Pure, World release (no title update). The runtime checks it before using it.

Before you start
----------------
The first time you open your game, its code is translated and compiled on this computer
(about 10 to 15 minutes, once). That needs a C++ compiler:

  macOS     xcode-select --install
  Windows   winget install LLVM.LLVM   (or the installer from llvm.org; let it add LLVM to PATH)
  Linux     sudo apt install clang     (or your distribution's clang package)

It also needs about 15 GB of free disk space for the game's files and the compiled game.

Playing
-------
1. Start pure-runtime (pure-runtime.exe on Windows).
     macOS: the first time, right-click it and choose Open: the build is not signed.
     Windows: if SmartScreen warns you, choose "More info", then "Run anyway".
2. Click "Open Pure disc image or default.xex..." and choose your disc image (.iso).
3. Wait while it copies the game's files and builds the game.
4. Next time, the window offers the same disc image: click Play, which starts at once. To use a
   different copy, click "Open Pure disc image or default.xex..." again.

Keyboard: W throttle, S brake, A/D steer, I/J/K/L right stick, Enter or Space = A,
Backspace = B, X, Y, Q/E shoulders, Escape = Start, Tab = Back, arrow keys = D-pad.
A gamepad works too.

Your saves and the prepared game live in:
  macOS     ~/Library/Application Support/pure-runtime
  Windows   %APPDATA%\pure-runtime
  Linux     ~/.local/share/pure-runtime

To build the game in a terminal instead, with progress printed there:
pure-runtime <your.iso> --build-game

Licenses: see LICENSE (this project, MIT), THIRD_PARTY_NOTICES.md and licenses/.
Pure is a trademark of its owner. This project is not affiliated with Disney, Black Rock Studio
or Microsoft.
