# Records docs/demo.gif with vhs (https://github.com/charmbracelet/vhs): `vhs docs/demo.tape`.
# The demo mode shows made-up sessions, so nothing real is recorded.
Output docs/demo.gif
Set Shell "bash"
Set FontSize 12
Set Width 1440
Set Height 820
Set Padding 0
Set Framerate 12
Set TypingSpeed 0
Env COLORTERM "truecolor"

Hide
Type "PS1='' && clear && ./target/release/ccorbit --demo" Enter
Sleep 300ms
Show
Sleep 7s
Right
Sleep 1s
Enter
Sleep 5s
Escape
Type "g"
Sleep 6s
Type "d"
Sleep 9s
Type "d"
Type "c"
Sleep 5s
Right
Sleep 800ms
Right
Sleep 2s
Hide
Type "q"
