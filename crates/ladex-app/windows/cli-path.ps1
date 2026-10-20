# Adds or removes a folder on the machine-wide PATH, keeping entries such as %SystemRoot% unexpanded.
param(
    [Parameter(Mandatory)][ValidateSet('Add', 'Remove')][string]$Action,
    [Parameter(Mandatory)][string]$Folder
)

$ErrorActionPreference = 'Stop'
$environmentKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
$key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($environmentKey, $true)
try {
    $current = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $entries = @($current -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ne $Folder.TrimEnd('\') })
    if ($Action -eq 'Add') {
        $entries += $Folder
    }
    $key.SetValue('Path', ($entries -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
}
finally {
    $key.Close()
}
