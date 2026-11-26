# Démo du README anglais : comme demo.tape, sortie en anglais (VIRTUM_LANG=en).
# Régénérer (environ 2 min) : depuis la racine du dépôt, avec virtum dans le PATH,
#   vhs assets/demo.en.tape
# puis supprimer le lab : (cd assets/demo && virtum down --destroy)
Output assets/demo.en.gif

Set Shell "bash"
Set FontSize 20
Set Width 1100
Set Height 720
Set Padding 24
Set Theme "GitHub Dark"
Set TypingSpeed 200ms
Set PlaybackSpeed 4
Set WaitTimeout 400s
Set WaitPattern /\$\s*$/

Env VIRTUM_LANG "en"

Hide
Type "cd assets/demo && export PS1='\$ ' && clear"
Enter
Show

Sleep 2s
Type "cat Virtumfile"
Enter
Sleep 12s

Type "virtum up"
Enter
Wait
Sleep 14s

Type "curl -s localhost:18080 | grep '<title>'"
Enter
Sleep 10s

Type "virtum shell web"
Enter
Wait
Sleep 6s
Type "systemctl is-active nginx"
Enter
Sleep 8s
Type "exit"
Enter
Wait
Sleep 12s
