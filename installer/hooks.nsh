; The installer runs elevated, so it adds the firewall rule HomePods need
; (UDP clock sync and TCP remote control, from the local network, this program
; only) and removes it on uninstall.

!macro NSIS_HOOK_POSTINSTALL
  nsExec::Exec 'netsh advfirewall firewall delete rule name="Ho My Pod (AirPlay)"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="Ho My Pod (AirPlay)" dir=in action=allow program="$INSTDIR\ho-my-pod.exe" protocol=any remoteip=localsubnet profile=any enable=yes'
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  nsExec::Exec 'netsh advfirewall firewall delete rule name="Ho My Pod (AirPlay)"'
!macroend
