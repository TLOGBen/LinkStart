$ErrorActionPreference = 'Stop'
$p = $env:LINKSTART_STORAGE_PATH
$isDir = [System.IO.Directory]::Exists($p)
$sections = [System.Security.AccessControl.AccessControlSections]'Access,Owner'
$a = if ($isDir) { [System.IO.Directory]::GetAccessControl($p,$sections) } else { [System.IO.File]::GetAccessControl($p,$sections) }
$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
$trusted = @($user.Value, 'S-1-5-18') # current account and LocalSystem only
if ($a.GetOwner([System.Security.Principal.SecurityIdentifier]).Value -notin $trusted) { throw 'untrusted runtime storage owner' }
foreach ($r in $a.GetAccessRules($true,$false,[System.Security.Principal.SecurityIdentifier])) {
    if ($r.AccessControlType -eq 'Allow' -and $r.IdentityReference.Value -notin $trusted) { throw 'untrusted explicit runtime storage ACE' }
}
$a.SetAccessRuleProtection($true,$false)
$r = if ($isDir) {
    [System.Security.AccessControl.FileSystemAccessRule]::new($user,'FullControl','ContainerInherit,ObjectInherit','None','Allow')
} else { [System.Security.AccessControl.FileSystemAccessRule]::new($user,'FullControl','Allow') }
$a.SetAccessRule($r)
if ($isDir) { [System.IO.Directory]::SetAccessControl($p,$a) } else { [System.IO.File]::SetAccessControl($p,$a) }
