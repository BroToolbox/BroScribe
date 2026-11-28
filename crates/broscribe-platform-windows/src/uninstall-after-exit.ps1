function Full-Path($path) {
  if ($path.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
    $path = '\\' + $path.Substring(8)
  } elseif ($path -match '^\\\\\?\\[A-Z]:\\') {
    $path = $path.Substring(4)
  }
  if ($path -notmatch '^([A-Z]:\\|\\\\[^\\]+\\[^\\]+(\\|$))') { throw 'A removal path is not absolute.' }
  $full = [IO.Path]::GetFullPath($path)
  if ($full.TrimEnd('\') -ne $path.TrimEnd('\')) { throw 'A removal path changed when resolved.' }
  return $full
}

function Is-Within($path, $parent) {
  $path = $path.TrimEnd('\')
  $parent = $parent.TrimEnd('\')
  return $path.Equals($parent, [StringComparison]::OrdinalIgnoreCase) -or
    $path.StartsWith(($parent + '\'), [StringComparison]::OrdinalIgnoreCase)
}

function Attributes($path) {
  try { return [IO.File]::GetAttributes($path) }
  catch [IO.FileNotFoundException] { return $null }
  catch [IO.DirectoryNotFoundException] { return $null }
}

function Assert-DirectoryPath($path) {
  if (!$path -or ![IO.Path]::GetDirectoryName($path)) { throw 'Refusing to remove a drive or share root.' }
  for ($part = $path; $part; $part = [IO.Path]::GetDirectoryName($part)) {
    $attributes = Attributes $part
    if ($null -ne $attributes -and (
      ($attributes -band [IO.FileAttributes]::ReparsePoint) -or
      !($attributes -band [IO.FileAttributes]::Directory)
    )) { throw ('A removal path is no longer a regular directory: ' + $part) }
  }
}

function Assert-ReleaseInventory($root, $plan) {
  if (([IO.File]::GetAttributes($plan) -band ([IO.FileAttributes]::Directory -bor [IO.FileAttributes]::ReparsePoint)) -or
      ([IO.FileInfo]::new($plan)).Length -gt 268435456) {
    throw 'The release inventory is not a regular, bounded file.'
  }
  $expected = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
  foreach ($file in ([IO.File]::ReadAllText($plan) | ConvertFrom-Json)) {
    $expected.Add($file.path.Replace('/', '\'), $file)
  }
  $queue = [Collections.Generic.Queue[string]]::new()
  $queue.Enqueue($root)
  while ($queue.Count) {
    $path = $queue.Dequeue()
    $relative = if ($path.Length -eq $root.Length) { '' } else { $path.Substring($root.Length + 1) }
    $attr = Attributes $path
    if ($null -eq $attr -or ($attr -band [IO.FileAttributes]::ReparsePoint)) {
      throw ('The release entry is missing or changed: ' + $path)
    }
    $directory = [bool]($attr -band [IO.FileAttributes]::Directory)
    if (!$expected.ContainsKey($relative)) {
      if (!$directory -and ($env:BROSCRIBE_UNINSTALL_OPTIONAL_FILES.Split(',') -contains $relative)) { continue }
      throw ('Unexpected release entry: ' + $path)
    }
    $entry = $expected[$relative]
    if ([bool]($attr -band [IO.FileAttributes]::ReadOnly) -ne [bool]$entry.mode) {
      throw ('Release entry attributes changed: ' + $path)
    }
    if ($directory -and $entry.type -ceq 'directory') {
      foreach ($child in [IO.Directory]::EnumerateFileSystemEntries($path)) { $queue.Enqueue($child) }
    } elseif (!$directory -and $entry.type -ceq 'file') {
      $stream = [IO.File]::OpenRead($path)
      $sha = [Security.Cryptography.SHA256]::Create()
      try {
        $digest = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '')
        if ($stream.Length -ne $entry.size -or $digest -ne $entry.sha256) {
          throw ('Release file changed: ' + $path)
        }
      } finally {
        $sha.Dispose()
        $stream.Dispose()
      }
    } else {
      throw ('Release entry type changed: ' + $path)
    }
    [void]$expected.Remove($relative)
  }
  if ($expected.Count) { throw ('Release entries are missing at ' + $root) }
}

function Assert-ReleaseTree($root) {
  Assert-DirectoryPath $root
  if (![IO.Directory]::Exists($root)) { return }
  if ($env:BROSCRIBE_UNINSTALL_RELEASE_INVENTORY) {
    Assert-ReleaseInventory $root (Full-Path $env:BROSCRIBE_UNINSTALL_RELEASE_INVENTORY)
    return
  }
  $required = $env:BROSCRIBE_UNINSTALL_RELEASE_FILES.Split(',')
  $allowed = $env:BROSCRIBE_UNINSTALL_ALLOWED_FILES.Split(',')
  $seen = @{}
  $unexpected = @()
  $queue = [Collections.Generic.Queue[string]]::new()
  $queue.Enqueue($root)
  while ($queue.Count -and $unexpected.Count -lt 4) {
    $folder = $queue.Dequeue()
    foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($folder)) {
      $name = $entry.Substring($root.Length + 1).Replace('\', '/')
      $attributes = Attributes $entry
      if (!$seen.ContainsKey($name) -and $null -ne $attributes -and
          !($attributes -band [IO.FileAttributes]::ReparsePoint)) {
        $seen[$name] = $true
        if ($attributes -band [IO.FileAttributes]::Directory) {
          $prefix = $name + '/'
          $allowedDirectory = $false
          foreach ($file in $allowed) {
            if ($file.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
              $queue.Enqueue($entry)
              $allowedDirectory = $true
              break
            }
          }
          if ($allowedDirectory) { continue }
        } elseif ($allowed -contains $name) {
          continue
        }
      }
      $unexpected += $entry
      if ($unexpected.Count -ge 4) { break }
    }
  }
  if ($unexpected.Count) {
    throw ('Unexpected installation entries: ' + ($unexpected -join ', ') + '. Move them out of ' + $root + ' before retrying.')
  }
  foreach ($name in $required) {
    if (!$seen.ContainsKey($name)) { throw ('Missing installation file: ' + $name) }
  }
}

function Assert-ProfileIdentity($data, $identity, $marker) {
  Assert-DirectoryPath $data
  if (![IO.Directory]::Exists($data)) { return }
  if ([BroScribeUninstallProfile]::Identity($data) -cne $identity) {
    throw ('The profile directory changed while waiting for exit: ' + $data)
  }
  $path = [IO.Path]::Combine($data, '.broscribe-profile')
  $attr = Attributes $path
  if ($null -eq $attr -and !$marker) { return }
  if ($null -eq $attr -or
      ($attr -band ([IO.FileAttributes]::Directory -bor [IO.FileAttributes]::ReparsePoint)) -or
      ([IO.FileInfo]::new($path)).Length -gt 256) {
    throw ('The BroScribe profile marker changed: ' + $data)
  }
  $record = [IO.File]::ReadAllText($path) | ConvertFrom-Json
  if (!$marker -or $record.schema -ne 1 -or $record.id -cne $marker) {
    throw ('The BroScribe profile marker changed: ' + $data)
  }
}

function Assert-ProtectedPaths($target, $overlap, $contains) {
  Assert-DirectoryPath $target
  foreach ($path in $overlap -split "`n") {
    if (!$path) { continue }
    $other = Full-Path $path
    if ((Is-Within $other $target) -or (Is-Within $target $other)) {
      throw ('Profile removal overlaps a protected location: ' + $other)
    }
  }
  foreach ($path in $contains -split "`n") {
    if ($path -and (Is-Within (Full-Path $path) $target)) {
      throw ('Profile removal contains a protected location: ' + $path)
    }
  }
}

function Assert-ProfileTarget($target) {
  Assert-ProtectedPaths $target $env:BROSCRIBE_UNINSTALL_PROTECT_OVERLAP $env:BROSCRIBE_UNINSTALL_PROTECT_CONTAINS
}

function Has-Type($path, $wantDirectory) {
  $attr = Attributes $path
  return ($null -ne $attr -and !($attr -band [IO.FileAttributes]::ReparsePoint) -and
    ([bool]($attr -band [IO.FileAttributes]::Directory) -eq $wantDirectory))
}

function Remove-HelperReference($reference, $receipt) {
  if (!$reference) { return }
  for ($attempt = 0; $attempt -lt 3; $attempt++) {
    $action = 'checked'
    try {
      $reference = Full-Path $reference
      $parent = [IO.Path]::GetDirectoryName($reference)
      Assert-DirectoryPath $parent
      $attr = Attributes $reference
      if ($null -eq $attr) { return }
      if (($attr -band ([IO.FileAttributes]::Directory -bor [IO.FileAttributes]::ReparsePoint)) -or
          ([IO.FileInfo]::new($reference)).Length -gt 262144) {
        throw 'The helper reference is not a regular, bounded file.'
      }
      # Check for one JSON string before wrapping it for Windows PowerShell.
      $json = [IO.File]::ReadAllText($reference)
      if ($json -cnotmatch '\A[ \t\r\n]*"(?:[^"\\\x00-\x1f]|\\(?:["\\/bfnrt]|u[0-9a-fA-F]{4}))*"[ \t\r\n]*\z') {
        throw 'The helper reference does not contain a JSON path string.'
      }
      $saved = ('{"receipt":' + $json + '}') | ConvertFrom-Json
      if (![StringComparer]::OrdinalIgnoreCase.Equals((Full-Path $saved.receipt), (Full-Path $receipt))) {
        return
      }
      Assert-DirectoryPath $parent
      if (!(Has-Type $reference $false)) { throw 'The helper reference changed while being checked.' }
      $action = 'removed'
      [IO.File]::Delete($reference)
      return @{ removed = $reference }
    } catch {
      $cause = $_.Exception.GetBaseException()
      # Win32 sharing and lock violations can clear while another handle closes.
      $retry = ($cause -is [IO.IOException]) -and (($cause.HResult -band 0xffff) -in @(32, 33))
      if (!$retry -or $attempt -eq 2) {
        return @{ kept = "kept: $reference (could not be ${action}: $($_.Exception.Message))" }
      }
      Start-Sleep -Milliseconds 250
    }
  }
}

function Assert-NoLibraryHere($folder) {
  $index = [IO.Path]::Combine($folder, '.index')
  $detected = (Has-Type $index $true) -and (
    (Has-Type ([IO.Path]::Combine($index, 'broscribe.sqlite')) $false) -or (
      (Has-Type ([IO.Path]::Combine($folder, 'people')) $true) -and
      (Has-Type ([IO.Path]::Combine($folder, '.queue')) $true)
    ))
  if (!$detected) {
    foreach ($year in [IO.Directory]::EnumerateDirectories($folder)) {
      if ([IO.Path]::GetFileName($year) -cnotmatch '^[0-9]{4}$' -or !(Has-Type $year $true)) { continue }
      foreach ($month in [IO.Directory]::EnumerateDirectories($year)) {
        if ([IO.Path]::GetFileName($month) -cnotmatch '^[0-9]{2}$' -or !(Has-Type $month $true)) { continue }
        foreach ($meeting in [IO.Directory]::EnumerateDirectories($month)) {
          if ((Has-Type $meeting $true) -and (
            (Has-Type ([IO.Path]::Combine($meeting, 'meeting.json')) $false) -or
            (Has-Type ([IO.Path]::Combine($meeting, 'meeting.md')) $false)
          )) { $detected = $true; break }
        }
      }
    }
  }
  if ($detected) { throw ('The removal path contains a BroScribe library at ' + $folder) }
}

function Assert-NoNestedLibrary($folder) {
  Assert-NoLibraryHere $folder
  foreach ($entry in [IO.Directory]::EnumerateDirectories($folder)) {
    if (Has-Type $entry $true) { Assert-NoNestedLibrary $entry }
  }
}

function Fold-Ascii($text) {
  return [regex]::Replace($text, '[A-Z]', { param($letter) $letter.Value.ToLowerInvariant() })
}

function Test-BenchSet($name) {
  return ($name -cmatch '\A[A-Za-z0-9_-]{1,64}\z' -and (Fold-Ascii $name) -cne 'results')
}

function Test-Wildcard($token, $id) {
  switch -CaseSensitive ($token) {
    '*' { return $id -cmatch '\A[A-Za-z0-9_-]+\z' }
    '#' {
      $generation = [uint64]0
      return ($id -cmatch '\A(?:0|[1-9][0-9]{0,19})\z' -and
        [uint64]::TryParse($id, [ref]$generation))
    }
    '>' {
      $pidValue = [uint32]0
      return ($id -cmatch '\A(?:0|[1-9][0-9]{0,9})\z' -and
        [uint32]::TryParse($id, [ref]$pidValue))
    }
    '~' { return $id -cmatch '\A[0-9A-Fa-f]{64}\z' }
    '%' { return $id -cmatch '\A[0-9.TtZz-]+\z' }
    '@' { return ($id -cnotin @('.', '..') -and $id -cmatch '\A[A-Za-z0-9._-]+\z') }
    '+' { return $id -cmatch '\A[A-Za-z0-9-]{1,40}\z' }
    '&' { return Test-BenchSet $id }
    '?' { return $id -cmatch '\A[0-9A-Fa-f]{64}\.[0-9A-Fa-f]{32}\z' }
    '!' {
      if ($id -cnotmatch '\A[0-9]{8}[Tt][0-9]{6}-(?<set>.+)\z') { return $false }
      $set = $Matches.set
      if (Test-BenchSet $set) { return $true }
      if ($set -cnotmatch '\A(?<base>.+)-(?<number>[1-9][0-9]{0,9})\z') { return $false }
      $number = [uint64]$Matches.number
      return ($number -ge 2 -and $number -le [uint32]::MaxValue -and (Test-BenchSet $Matches.base))
    }
    '^' { return $id -cmatch '\A[0-9a-f]{6}\z' }
    '=' {
      $sequence = [uint64]0
      return ($id -cmatch '\A[0-9]{6,20}\z' -and [uint64]::TryParse($id, [ref]$sequence) -and
        $id -ceq $sequence.ToString('D6', [Globalization.CultureInfo]::InvariantCulture))
    }
    ':' { return $id -cmatch '\A[A-Za-z0-9]{1,8}\z' }
    '$' { return $id -cmatch '\A[A-Za-z0-9]{6}\z' }
    ';' { return $id -cmatch '\A[0-9]{4}\z' }
    '{' { return $id -cmatch '\A[0-9]{2}\z' }
    '[' { return $id -cmatch '\A[0-9a-f]{16}\z' }
    ']' {
      if ($id -cnotmatch '\Aj-[0-9a-f]{16}-(?<stamp>-?[0-9]+)\z') { return $false }
      $stamp = [int64]0
      return ([int64]::TryParse($Matches.stamp, [ref]$stamp) -and
        $Matches.stamp -ceq $stamp.ToString([Globalization.CultureInfo]::InvariantCulture))
    }
    '}' { return $id -cmatch '\A[0-9a-f]{32}\.[A-Za-z0-9]+\z' }
  }
  return $false
}

function Matches-Shape($name, $patterns) {
  $parts = $name.Split('/')
  $tokens = '*#~%@+&!?^=:$;{[]}>'.ToCharArray()
  foreach ($pattern in $patterns) {
    $components = $pattern.Split('/')
    if ($components.Count -ne $parts.Count) { continue }
    $matched = $true
    for ($i = 0; $i -lt $components.Count; $i++) {
      $component = $components[$i]
      $part = $parts[$i]
      $offset = $component.IndexOfAny($tokens)
      if ($offset -lt 0) {
        if (!(Fold-Ascii $part).Equals((Fold-Ascii $component), [StringComparison]::Ordinal)) {
          $matched = $false
          break
        }
        continue
      }
      $prefix = Fold-Ascii ($component.Substring(0, $offset))
      $suffix = Fold-Ascii ($component.Substring($offset + 1))
      $folded = Fold-Ascii $part
      $length = $part.Length - $prefix.Length - $suffix.Length
      if ($length -le 0 -or !$folded.StartsWith($prefix, [StringComparison]::Ordinal) -or
          !$folded.EndsWith($suffix, [StringComparison]::Ordinal) -or
          !(Test-Wildcard ([string]$component[$offset]) ($part.Substring($offset, $length)))) {
        $matched = $false
        break
      }
    }
    if ($matched) { return $true }
  }
  return $false
}

function Atomic-Target($name) {
  if ($name -cmatch '\A(?<parent>(?:.*/)?)\.(?<target>[^/]+)\.tmp-(?<pid>[0-9]+)-[0-9a-f]{8}\z') {
    $writer = 0
    if ([uint32]::TryParse($Matches.pid, [ref]$writer)) { return $Matches.parent + $Matches.target }
  }
}

function Matches-Literal($name, $literals) {
  $name = Fold-Ascii $name
  foreach ($literal in $literals) {
    if ([StringComparer]::Ordinal.Equals($name, (Fold-Ascii $literal))) { return $true }
  }
  return $false
}

function Matches-CleanupRule($name, $shape, $type) {
  if ($type -eq 'file') {
    $direct = if ($shape.literal) { Matches-Literal $name $shape.files } else { Matches-Shape $name $shape.files }
    if ($direct) { return $true }
    $target = Atomic-Target $name
    if (!$target) { return $false }
    if ($shape.literal) { return Matches-Literal $target $shape.atomic }
    return Matches-Shape $target $shape.atomic
  }
  if ($type -eq 'dir') {
    if ($shape.literal) { return Matches-Literal $name $shape.dirs }
    return Matches-Shape $name $shape.dirs
  }
  return Matches-Shape $name $shape.$type
}

function Read-ModelRecord($path, $id) {
  if (!(Has-Type $path $false) -or ([IO.FileInfo]::new($path)).Length -gt 4194304) { return }
  try { $record = [IO.File]::ReadAllText($path) | ConvertFrom-Json }
  catch { return }
  if ($record.id -ceq $id -and $null -ne $record.files) { return $record }
}

function Add-ModelPath($model, $name, $directory) {
  if ($directory) { $model.dirs += $name } else { $model.files += $name }
  for ($slash = $name.IndexOf('/'); $slash -ge 0; $slash = $name.IndexOf('/', $slash + 1)) {
    $model.dirs += $name.Substring(0, $slash)
  }
}

$modelBadPath = '(^|/)(\.|\.\.)?(/|$)|\\'

# The staging record of a model folder, if cleanup accepts it as that model's own.
function Read-StagingRecord($folder) {
  $partial = [IO.Path]::Combine($folder, '.partial')
  if (!(Has-Type $partial $true)) { return }
  $entries = Read-ModelRecord ([IO.Path]::Combine($partial, '.entries.json')) ([IO.Path]::GetFileName($folder))
  if (!$entries) { return }
  $paths = @($entries.files) + @($entries.dirs)
  if ($null -ne $entries.sources) { $paths += @($entries.sources) }
  if ($null -ne $entries.moved) { $paths += @($entries.moved) }
  if (@($paths -match $modelBadPath).Count -eq 0) { return $entries }
}

function Read-ModelShape($folder) {
  $id = [IO.Path]::GetFileName($folder)
  $model = @{ files = @(); dirs = @('.partial'); atomic = @('.verified', '.partial/.entries.json'); literal = $true }
  $record = Read-ModelRecord ([IO.Path]::Combine($folder, '.verified')) $id
  $badPath = $modelBadPath
  if ($record -and !((@($record.files | ForEach-Object { $_.path }) + @($record.sources | ForEach-Object { $_.file })) -match $badPath)) {
    $model.files += '.verified'
    # The marker lists installed files only. Staging belongs to the download's own record.
    foreach ($file in $record.files) { Add-ModelPath $model $file.path $false }
  }
  $partial = [IO.Path]::Combine($folder, '.partial')
  $entries = Read-StagingRecord $folder
  if ($entries) {
    $movedNames = @()
    if ($null -ne $entries.moved) { $movedNames = @($entries.moved) }
    $model.files += '.partial/.entries.json'
    # Install records each top-level entry before renaming it out of
    # staging. A root name is BroScribe's only once that entry has moved.
    $moved = {
      param($name)
      $top = $name.Split('/')[0]
      $staged = [IO.Path]::Combine($partial, $top)
      ($movedNames -ccontains $top) -and !([IO.File]::Exists($staged) -or [IO.Directory]::Exists($staged))
    }
    foreach ($file in $entries.files) {
      Add-ModelPath $model ('.partial/' + $file) $false
      if (& $moved $file) { Add-ModelPath $model $file $false }
    }
    foreach ($dir in $entries.dirs) {
      Add-ModelPath $model ('.partial/' + $dir) $true
      if (& $moved $dir) { Add-ModelPath $model $dir $true }
    }
    foreach ($source in $entries.sources) { Add-ModelPath $model ('.partial/' + $source) $false }
  }
  return $model
}

function Assert-BackupLayout($folder) {
  $files = $env:BROSCRIBE_UNINSTALL_BACKUP_FILES.Split(',')
  $queue = [Collections.Generic.Queue[string]]::new()
  $queue.Enqueue($folder)
  while ($queue.Count) {
    $dir = $queue.Dequeue()
    foreach ($path in [IO.Directory]::EnumerateFileSystemEntries($dir)) {
      $name = $path.Substring($folder.Length + 1).Replace('\', '/')
      if ((Has-Type $path $false) -and $name -cin $files) { continue }
      if ((Has-Type $path $true) -and ($files | Where-Object { $_.StartsWith($name + '/', [StringComparison]::Ordinal) })) {
        $queue.Enqueue($path)
        continue
      }
      throw ('Unexpected app backup entry: ' + $path)
    }
  }
}

function Plan-Owned($folder, $shape, $prefix, $skip, $tasks) {
  Assert-NoLibraryHere $folder
  foreach ($child in [IO.Directory]::EnumerateFileSystemEntries($folder)) {
    $name = [IO.Path]::GetFileName($child)
    if (!$prefix -and $name -in $skip) { continue }
    $relative = $prefix + $name
    $fileMatch = Matches-CleanupRule $relative $shape 'file'
    $dirMatch = Matches-CleanupRule $relative $shape 'dir'
    if ((Has-Type $child $false) -and $fileMatch -and !($shape -eq $shapes.run -and $name.EndsWith('.sock', [StringComparison]::OrdinalIgnoreCase))) {
      $tasks.Add(@{ path = $child; kind = 'file' })
    } elseif ((Has-Type $child $true) -and (Matches-CleanupRule $relative $shape 'models')) {
      $model = Read-ModelShape $child
      if ($model) {
        Plan-Owned $child $model '' @() $tasks
      } elseif ($dirMatch) { Plan-Owned $child $shape ($relative + '/') $skip $tasks }
      else { $kept.Add($child) }
    } elseif ((Has-Type $child $true) -and (Matches-CleanupRule $relative $shape 'trees')) {
      Assert-NoNestedLibrary $child
      Assert-BackupLayout $child
      $tasks.Add(@{ path = $child; kind = 'tree' })
    } elseif ((Has-Type $child $true) -and $dirMatch) {
      Plan-Owned $child $shape ($relative + '/') $skip $tasks
    } else {
      $kept.Add($child)
    }
  }
  $tasks.Add(@{ path = $folder; kind = 'empty' })
}

function Get-KeptLines {
  $kept | Sort-Object -Unique | ForEach-Object { "kept: not created by BroScribe: " + $_ }
  $keptParents | Sort-Object -Unique | ForEach-Object { "kept: directory containing an unrecognized entry: " + $_ }
}

function Write-CleanupReceipt($path, $status, $lines) {
  $encoding = [Text.UTF8Encoding]::new($false)
  $limit = 65536 - 128
  $used = 0
  $omitted = 0
  $builder = [Text.StringBuilder]::new()
  foreach ($line in $lines) {
    $bytes = $encoding.GetByteCount([string]$line) + 1
    if ($bytes -gt ($limit - $used)) { $omitted++; continue }
    [void]$builder.Append($line).Append("`n")
    $used += $bytes
  }
  if ($omitted) { [void]$builder.Append("Omitted $omitted additional detail lines.`n") }
  if ($builder.Length) {
    try {
      $file = [IO.File]::Open(($path + '.details'), [IO.FileMode]::CreateNew,
        [IO.FileAccess]::Write, [IO.FileShare]::Read)
      try {
        $bytes = $encoding.GetBytes($builder.ToString())
        $file.Write($bytes, 0, $bytes.Length)
      } finally { $file.Dispose() }
    } catch {
      $status += ' The details file could not be saved.'
    }
  }
  [IO.File]::WriteAllText($path, $status, $encoding)
}

# Takes staged entries about to be deleted off their records' moved lists
# first, so a staged copy removed by an interrupted cleanup never reads as
# one that install moved to the model root.
# Only staging folders inside the cleaned folder, with accepted records, change.
function Clear-ModelMoves($folder, $tasks) {
  $staged = @{}
  foreach ($task in $tasks) {
    for ($child = $task.path; $child; $child = $parent) {
      $parent = [IO.Path]::GetDirectoryName($child)
      if (!$parent -or !(Is-Within $parent $folder)) { break }
      if ([IO.Path]::GetFileName($parent) -ceq '.partial') {
        $name = [IO.Path]::GetFileName($child)
        if ($name -cne '.entries.json') {
          if (!$staged.ContainsKey($parent)) { $staged[$parent] = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal) }
          [void]$staged[$parent].Add($name)
        }
        break
      }
    }
  }
  foreach ($partial in $staged.Keys) {
    $file = [IO.Path]::Combine($partial, '.entries.json')
    $modelFolder = [IO.Path]::GetDirectoryName($partial)
    if (!(Is-Within $modelFolder $folder)) { continue }
    $record = Read-StagingRecord $modelFolder
    if (!$record -or $null -eq $record.moved) { continue }
    $remaining = @(@($record.moved) | Where-Object { !$staged[$partial].Contains([string]$_) })
    if ($remaining.Count -eq @($record.moved).Count) { continue }
    $record.moved = $remaining
    [IO.File]::WriteAllText($file, (ConvertTo-Json $record -Compress -Depth 4), [Text.UTF8Encoding]::new($false))
  }
}

function Remove-Owned($folder, $kind, $skip) {
  if ($null -eq (Attributes $folder)) { return }
  if (!(Has-Type $folder $true)) { $kept.Add($folder); return }
  Assert-DirectoryPath $folder
  $tasks = [Collections.Generic.List[object]]::new()
  $first = $kept.Count
  Plan-Owned $folder $shapes.$kind '' $skip $tasks
  for ($i = $first; $i -lt $kept.Count; $i++) {
    for ($parent = [IO.Path]::GetDirectoryName($kept[$i]); $parent -and (Is-Within $parent $folder); $parent = [IO.Path]::GetDirectoryName($parent)) {
      $keptParents.Add($parent)
    }
  }
  Clear-ModelMoves $folder $tasks
  foreach ($task in $tasks) {
    if ($task.kind -eq 'file' -and (Has-Type $task.path $false)) {
      [IO.File]::Delete($task.path)
    } elseif ((Has-Type $task.path $true)) {
      if ($task.kind -eq 'tree') {
        Assert-NoNestedLibrary $task.path
        Assert-BackupLayout $task.path
        Remove-Tree $task.path
      } elseif ($task.kind -eq 'empty' -and
          !([IO.Directory]::EnumerateFileSystemEntries($task.path) | Select-Object -First 1)) {
        [IO.Directory]::Delete($task.path, $false)
      }
    }
  }
}

function Get-DataCleanupNotes {
  $notes = foreach ($folder in ($dataTargets | Sort-Object -Unique)) {
    if ($null -eq (Attributes $folder)) { continue }
    "partial: data folder remains: " + $folder
    if ($null -ne (Attributes ([IO.Path]::Combine($folder, 'updates')))) {
      foreach ($name in @('config.toml', '.broscribe-profile', 'uninstall-pending.json', 'updates')) {
        $path = [IO.Path]::Combine($folder, $name)
        if ($null -ne (Attributes $path)) { "kept: update and uninstall recovery: " + $path }
      }
    }
  }
  return $notes
}

function Test-JournalDirectory($path) {
  $attributes = Attributes $path
  if ($null -ne $attributes -and ($attributes -band [IO.FileAttributes]::ReparsePoint)) {
    throw ('Cannot verify journals at ' + $path + ' because the path is a link.')
  }
  return ($null -ne $attributes -and [bool]($attributes -band [IO.FileAttributes]::Directory))
}

function Assert-NoUnsavedJournals($folder) {
  $discard = $env:BROSCRIBE_UNINSTALL_DISCARD_UNSAVED -eq '1'
  $journal = [IO.Path]::Combine($folder, 'journal')
  if (!(Test-JournalDirectory $journal)) { return }
  $message = 'This profile holds unsaved or processing recordings. Save them first, or use --remove-data --discard-unsaved.'
  foreach ($session in [IO.Directory]::EnumerateFileSystemEntries($journal)) {
    if (!(Test-JournalDirectory $session) -or $discard) { continue }
    if ([IO.Path]::GetFileName($session) -ieq '.discard') {
      foreach ($intent in [IO.Directory]::EnumerateFileSystemEntries($session)) {
        if ([IO.Path]::GetExtension($intent) -ceq '.json' -and
            (Test-Wildcard '*' ([IO.Path]::GetFileNameWithoutExtension($intent)))) { throw $message }
      }
      continue
    }
    if (Has-Type ([IO.Path]::Combine($session, 'session.json')) $false) { throw $message }
    foreach ($track in @('mic', 'system')) {
      $trackPath = [IO.Path]::Combine($session, $track)
      if (!(Test-JournalDirectory $trackPath)) { continue }
      foreach ($chunk in [IO.Directory]::EnumerateFiles($trackPath)) {
        if ((Matches-Shape ([IO.Path]::GetFileName($chunk)) @('=.f32')) -and (Has-Type $chunk $false)) {
          throw ('Unsaved or processing recordings may remain at ' + $session +
            '. Move this folder away, or use --remove-data --discard-unsaved to delete its BroScribe journal files.')
        }
      }
    }
  }
}

function Remove-ProfileData($folder) {
  Assert-NoUnsavedJournals $folder
  $dataTargets.Add($folder)
  Remove-Owned ([IO.Path]::Combine($folder, 'run')) 'run'
  $skip = $dataSeparate
  if ($null -ne (Attributes ([IO.Path]::Combine($folder, 'updates')))) {
    $skip = $dataWithState
  }
  Remove-Owned $folder 'data' $skip
}

function Clear-DefaultProfile($deadline) {
  if (!$env:BROSCRIBE_UNINSTALL_DEFAULT) { return }
  $default = Full-Path $env:BROSCRIBE_UNINSTALL_DEFAULT
  Assert-ProfileIdentity $default $env:BROSCRIBE_UNINSTALL_DEFAULT_ID $env:BROSCRIBE_UNINSTALL_DEFAULT_MARKER
  if ($env:BROSCRIBE_UNINSTALL_DELETE_DEFAULT -eq '1') {
    Assert-ProtectedPaths $default $env:BROSCRIBE_UNINSTALL_DEFAULT_OVERLAP $env:BROSCRIBE_UNINSTALL_DEFAULT_CONTAINS
    $run = [IO.Path]::Combine($default, 'run')
    Assert-DirectoryPath $run
    Wait-AppLock ([IO.Path]::Combine($run, 'app.lock')) $deadline
    Assert-ProfileIdentity $default $env:BROSCRIBE_UNINSTALL_DEFAULT_ID $env:BROSCRIBE_UNINSTALL_DEFAULT_MARKER
    Remove-ProfileData $default
    return $default
  }
  $pending = [IO.Path]::Combine($default, 'uninstall-pending.json')
  $attributes = Attributes $pending
  if ($null -ne $attributes) {
    if ($attributes -band ([IO.FileAttributes]::Directory -bor [IO.FileAttributes]::ReparsePoint)) {
      throw 'The pending uninstall list is no longer a regular file.'
    }
    [IO.File]::Delete($pending)
  }
  if ($env:BROSCRIBE_UNINSTALL_CREATED_DEFAULT -eq '1' -and [IO.Directory]::Exists($default)) {
    Assert-ProfileIdentity $default $env:BROSCRIBE_UNINSTALL_DEFAULT_ID $env:BROSCRIBE_UNINSTALL_DEFAULT_MARKER
    if (!([IO.Directory]::EnumerateFileSystemEntries($default) | Select-Object -First 1)) {
      [IO.Directory]::Delete($default, $false)
    }
  }
  return $pending
}

function Remove-Tree($path) {
  $attributes = Attributes $path
  if ($null -eq $attributes) { return }
  if ($attributes -band [IO.FileAttributes]::Directory) {
    if (!($attributes -band [IO.FileAttributes]::ReparsePoint)) {
      foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($path)) {
        Remove-Tree $entry
      }
    }
    [IO.Directory]::Delete($path, $false)
  } else {
    [IO.File]::Delete($path)
  }
}

function Wait-AppLock($lock, $deadline) {
  while ($null -ne (Attributes $lock)) {
    if ((Attributes $lock) -band [IO.FileAttributes]::ReparsePoint) { throw 'The app lock changed while waiting.' }
    try {
      $file = [IO.File]::Open($lock, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
      $file.Dispose()
      return
    } catch [IO.IOException] {
      if ([DateTime]::UtcNow -ge $deadline) { throw 'The app still holds its profile files.' }
      Start-Sleep -Milliseconds 250
    }
  }
}

# Dot-sourcing exposes the functions without reading a removal plan or touching disk.
if ($MyInvocation.InvocationName -eq '.') { return }

$ErrorActionPreference = 'Stop'
$receipt = $env:BROSCRIBE_UNINSTALL_RECEIPT
$kept = [Collections.Generic.List[string]]::new()
$keptParents = [Collections.Generic.List[string]]::new()
$dataTargets = [Collections.Generic.List[string]]::new()
$dataSeparate = @('run', 'cache', 'updates', 'update-runner', 'app-backups')
$dataWithState = $dataSeparate + @('.broscribe-profile', 'uninstall-pending.json', 'config.toml')
$updateGuard = $null
try {
  $shapes = $env:BROSCRIBE_UNINSTALL_PROFILE_SHAPES | ConvertFrom-Json
  $deadline = [DateTime]::UtcNow.AddMinutes(15)
  foreach ($id in ([string]$env:BROSCRIBE_UNINSTALL_PROCESSES -split ',')) {
    if (!$id) { continue }
    $process = Get-Process -Id ([int]$id) -ErrorAction SilentlyContinue
    if ($process) {
      $ms = [Math]::Max(0, [int]($deadline - [DateTime]::UtcNow).TotalMilliseconds)
      if (!$process.WaitForExit($ms)) { throw 'A BroScribe process is still running.' }
    }
  }
  $root = $null
  if ($env:BROSCRIBE_UNINSTALL_ROOT) {
    $root = Full-Path $env:BROSCRIBE_UNINSTALL_ROOT
    Assert-ReleaseTree $root
    if ($env:BROSCRIBE_UNINSTALL_UPDATE_CLEANUP) {
      $parent = [IO.Path]::GetDirectoryName($root)
      $stage = [IO.Path]::Combine($parent, '.' + [IO.Path]::GetFileName($root) + '.update')
      $lock = $stage + '.lock'
      if ($null -ne (Attributes $lock) -and !(Has-Type $lock $false)) {
        throw ('The updater lock changed after uninstall started: ' + $lock)
      }
      $updateGuard = [IO.File]::Open($lock, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
      if ($null -ne (Attributes $stage)) {
        throw ('Update staging reappeared. Retry uninstall after broscribe update recover: ' + $stage)
      }
    }
  }
  $target = $null
  $runner = $null
  if ($env:BROSCRIBE_UNINSTALL_PROFILE -or $env:BROSCRIBE_UNINSTALL_UPDATE_CLEANUP) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class BroScribeUninstallProfile {
  [StructLayout(LayoutKind.Sequential)]
  struct FileInfo {
    public uint Attributes, CreatedLow, CreatedHigh, AccessLow, AccessHigh, WriteLow, WriteHigh;
    public uint Volume, SizeHigh, SizeLow, Links, IndexHigh, IndexLow;
  }
  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  static extern SafeFileHandle CreateFile(string path, uint access, uint share, IntPtr security,
      uint disposition, uint flags, IntPtr template);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool GetFileInformationByHandle(SafeFileHandle file, out FileInfo info);
  public static string Identity(string path) {
    using (var file = CreateFile(path, 0, 7, IntPtr.Zero, 3, 0x02200000, IntPtr.Zero)) {
      FileInfo info;
      if (file.IsInvalid || !GetFileInformationByHandle(file, out info))
        throw new Win32Exception(Marshal.GetLastWin32Error());
      if ((info.Attributes & 0x400) != 0 || (info.Attributes & 0x10) == 0)
        throw new InvalidOperationException("The profile is no longer a regular directory.");
      ulong index = ((ulong)info.IndexHigh << 32) | info.IndexLow;
      ulong generation = ((ulong)info.CreatedHigh << 32) | info.CreatedLow;
      return String.Format("{0:x}:{1:x}:{2:x}", info.Volume, index, generation);
    }
  }
}
'@
  }
  if ($env:BROSCRIBE_UNINSTALL_PROFILE) {
    $data = Full-Path $env:BROSCRIBE_UNINSTALL_PROFILE
    $run = [IO.Path]::Combine($data, 'run')
    $target = $run
    if ($env:BROSCRIBE_UNINSTALL_DELETE_DATA -eq '1') { $target = $data }
    Assert-ProfileTarget $target
    if ($env:BROSCRIBE_UNINSTALL_DELETE_UPDATE_RUNNER -eq '1') {
      $runner = [IO.Path]::Combine($data, 'update-runner')
      Assert-ProfileTarget $runner
    }
    Assert-DirectoryPath $run
    if ($root -and ((Is-Within $root $data) -or (Is-Within $data $root))) {
      throw 'The installation overlaps the profile.'
    }
    Wait-AppLock ([IO.Path]::Combine($run, 'app.lock')) $deadline
    Assert-ProfileIdentity $data $env:BROSCRIBE_UNINSTALL_PROFILE_ID $env:BROSCRIBE_UNINSTALL_PROFILE_MARKER
    Assert-DirectoryPath $run
  }
  $removed = @()
  if ($target) {
    Assert-ProfileIdentity $data $env:BROSCRIBE_UNINSTALL_PROFILE_ID $env:BROSCRIBE_UNINSTALL_PROFILE_MARKER
    Assert-ProfileTarget $target
    if ($target -eq $data) { Remove-ProfileData $target }
    else { Remove-Owned $target 'run' }
    if ($null -eq (Attributes $target)) { $removed += $target }
  }
  if ($runner) {
    Assert-ProfileIdentity $data $env:BROSCRIBE_UNINSTALL_PROFILE_ID $env:BROSCRIBE_UNINSTALL_PROFILE_MARKER
    Assert-ProfileTarget $runner
    Remove-Owned $runner 'update-runner'
    if ($null -eq (Attributes $runner)) { $removed += $runner }
  }
  $removed += @(Clear-DefaultProfile $deadline | Where-Object { $null -eq (Attributes $_) })
  if ($root -and [IO.Directory]::Exists($root)) {
    Assert-ReleaseTree $root
    $retired = $root + '.uninstall-' + [Guid]::NewGuid().ToString('N')
    [IO.Directory]::Move($root, $retired)
    try {
      Assert-ReleaseTree $retired
    } catch {
      if ($null -eq (Attributes $root)) { [IO.Directory]::Move($retired, $root) }
      throw
    }
    for ($attempt = 0; $attempt -lt 3; $attempt++) {
      try {
        Remove-Tree $retired
        break
      } catch {
        if ($attempt -eq 2) { throw ('Remove remaining files from ' + $retired + '. ' + $_.Exception.Message) }
        Start-Sleep -Milliseconds 250
      }
    }
    if ($null -eq (Attributes $root)) { $removed += $root }
  }
  if ($root -and $null -eq (Attributes $root)) {
    foreach ($cleanup in ($env:BROSCRIBE_UNINSTALL_UPDATE_CLEANUP -split "`n")) {
      if (!$cleanup) { continue }
      $path, $identity, $marker, $removeData = $cleanup -split "`t", 4
      $profile = Full-Path $path
      Assert-ProfileIdentity $profile $identity $marker
      $updates = [IO.Path]::Combine($profile, 'updates')
      if ((Is-Within $root $updates) -or (Is-Within $updates $root)) {
        throw ('Update state overlaps the installation: ' + $updates)
      }
      Remove-Owned $updates 'updates'
      if ($null -eq (Attributes $updates)) {
        $removed += $updates
        if ($removeData -eq '1') {
          Assert-NoUnsavedJournals $profile
          $dataTargets.Add($profile)
          Remove-Owned $profile 'data' $dataSeparate
        }
      } elseif ($removeData -eq '1') {
        $dataTargets.Add($profile)
      }
    }
  }
  $removed += @($dataTargets | Sort-Object -Unique | Where-Object { $_ -notin $removed -and $null -eq (Attributes $_) })
  # Clear the reference before Done makes this run eligible for a CLI retry.
  $referenceCleanup = Remove-HelperReference $env:BROSCRIBE_UNINSTALL_REFERENCE $receipt
  if ($referenceCleanup.removed) { $removed += $referenceCleanup.removed }
  $status = "Done. Removed $($removed.Count) entries."
  $details = @($removed | ForEach-Object { "removed: " + $_ }) + @(Get-KeptLines) + @(Get-DataCleanupNotes)
  if ($referenceCleanup.kept) {
    $details = @($referenceCleanup.kept) + $details
    $status += ' Helper reference kept.'
  }
  Write-CleanupReceipt $receipt $status $details
} catch {
  $details = @($_.Exception.Message) + @(Get-KeptLines) + @(Get-DataCleanupNotes)
  if ($env:BROSCRIBE_UNINSTALL_UPDATE_CLEANUP) {
    $details += "Any remaining update state was kept for retry."
  }
  Write-CleanupReceipt $receipt 'Failed. See the uninstall details.' $details
  exit 1
} finally {
  if ($updateGuard) { $updateGuard.Dispose() }
  if ($env:BROSCRIBE_UNINSTALL_RELEASE_INVENTORY) {
    [IO.File]::Delete($env:BROSCRIBE_UNINSTALL_RELEASE_INVENTORY)
  }
  $scriptPath = $env:BROSCRIBE_UNINSTALL_SCRIPT
  if ($receipt -and [StringComparer]::OrdinalIgnoreCase.Equals($scriptPath,
      [IO.Path]::Combine([IO.Path]::GetDirectoryName($receipt), 'uninstall-after-exit.ps1'))) {
    [IO.File]::Delete($scriptPath)
  }
}
