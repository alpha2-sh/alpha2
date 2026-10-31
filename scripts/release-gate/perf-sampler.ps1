# Windows utilization sampler for the release gate's perf runner (perf.mjs --sample), for Windows
# PowerShell 5.1 or pwsh.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File perf-sampler.ps1 -Out <csv> -Stop <file>
#
# Samples until the stop file exists. Uses the CIM performance classes rather than Get-Counter
# paths, because counter paths are localized and the class and property names are not. One row per
# (sample, GPU engine type, adapter LUID): epoch, host pids, total CPU %, CPU performance %
# (turbo/throttle), host CPU % of the machine, engine type, LUID, utilization summed over the
# host's processes, utilization over all processes, dedicated and shared GPU memory of that LUID
# (bytes). On a laptop with an integrated and a discrete GPU, the LUID with dedicated memory is the
# discrete one.
param(
  [Parameter(Mandatory = $true)][string]$Out,
  [Parameter(Mandatory = $true)][string]$Stop,
  [int]$IntervalMs = 500
)
$ErrorActionPreference = 'SilentlyContinue'
$cores = [Environment]::ProcessorCount
'epoch,host_pids,cpu_total_pct,cpu_performance_pct,host_cpu_pct,engine,luid,host_util_pct,all_util_pct,dedicated_bytes,shared_bytes' |
  Out-File -FilePath $Out -Encoding ascii
while (-not (Test-Path -LiteralPath $Stop)) {
  $epoch = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() / 1000.0
  $cpu = (Get-CimInstance -ClassName Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'").PercentProcessorTime
  $perf = (Get-CimInstance -ClassName Win32_PerfFormattedData_Counters_ProcessorInformation -Filter "Name='_Total'").PercentProcessorPerformance
  $procs = @(Get-CimInstance -ClassName Win32_PerfFormattedData_PerfProc_Process -Filter "Name LIKE 'alpha2-host%'")
  $hostCpu = 0.0
  $pids = @()
  foreach ($p in $procs) {
    $hostCpu += [double]$p.PercentProcessorTime
    $pids += [int]$p.IDProcess
  }
  $hostCpu = $hostCpu / $cores
  $memory = @{}
  foreach ($m in @(Get-CimInstance -ClassName Win32_PerfFormattedData_GPUPerformanceCounters_GPUAdapterMemory)) {
    if ($m.Name -match '(luid_0x[0-9a-fA-F]+_0x[0-9a-fA-F]+)') {
      $memory[$matches[1].ToLower()] = @([double]$m.DedicatedUsage, [double]$m.SharedUsage)
    }
  }
  $groups = @{}
  foreach ($e in @(Get-CimInstance -ClassName Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine)) {
    if ($e.Name -match '^pid_(\d+)_(luid_0x[0-9a-fA-F]+_0x[0-9a-fA-F]+)_phys_\d+_eng_\d+_engtype_(.+)$') {
      $owner = [int]$matches[1]
      $key = $matches[3] + '|' + $matches[2].ToLower()
      if (-not $groups.ContainsKey($key)) { $groups[$key] = @(0.0, 0.0) }
      $value = [double]$e.UtilizationPercentage
      $groups[$key][1] += $value
      if ($pids -contains $owner) { $groups[$key][0] += $value }
    }
  }
  $pidText = ($pids -join ' ')
  foreach ($key in $groups.Keys) {
    $parts = $key.Split('|')
    $mem = $memory[$parts[1]]
    if ($null -eq $mem) { $mem = @(0, 0) }
    # Invariant culture: a locale with a decimal comma must not split the CSV fields.
    [string]::Format([Globalization.CultureInfo]::InvariantCulture,
      '{0:F3},{1},{2},{3},{4:F1},{5},{6},{7:F1},{8:F1},{9},{10}', $epoch, $pidText, $cpu, $perf, $hostCpu,
      $parts[0], $parts[1], $groups[$key][0], $groups[$key][1], $mem[0], $mem[1]) |
      Out-File -FilePath $Out -Append -Encoding ascii
  }
  Start-Sleep -Milliseconds $IntervalMs
}
