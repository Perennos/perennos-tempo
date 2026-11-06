# Perennos Tempo (offline): local copy of the Tempo GUI + the local agent behind it. Opens the GUI on the CHAT surface.
$home_='C:\LocalAI\tempo-agent'
function Up($u){try{Invoke-WebRequest $u -UseBasicParsing -TimeoutSec 2|Out-Null;$true}catch{$false}}
if(-not (Up 'http://127.0.0.1:11434/api/tags')){ Start-Process ollama -ArgumentList 'serve' -WindowStyle Hidden; Start-Sleep 4 }
if(-not (Up 'http://127.0.0.1:18795/')){ Start-Process 'C:\LocalAI\apps\perennos-tempo-desktop\tempo-gui.exe' -ArgumentList '--serve','--port','18795','--ui',"$home_\gui-ui" -WindowStyle Hidden; Start-Sleep 3 }
if(-not (Up 'http://127.0.0.1:8765/agent/convs')){ Start-Process pythonw -ArgumentList '-X','utf8',"$home_\tempo_agent.py" -WorkingDirectory $home_ -WindowStyle Hidden; Start-Sleep 3 }
Start-Process msedge -ArgumentList '--app=http://127.0.0.1:8765/#chat',"--user-data-dir=$home_\edge-profile"
