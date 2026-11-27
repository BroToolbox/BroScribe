$ErrorActionPreference = 'Stop'; & ([scriptblock]::Create([IO.File]::ReadAllText($env:BROSCRIBE_UNINSTALL_SCRIPT)))
