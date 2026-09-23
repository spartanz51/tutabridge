import sys
from pathlib import Path
from smoke_tuta_api import sdk_versions, check_version
sys.exit(check_version(*sdk_versions(Path(sys.argv[1]))))
