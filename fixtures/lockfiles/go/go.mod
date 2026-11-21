// Hand-written sample of a go.mod.
module example.com/demo

go 1.22.3

toolchain go1.22.5

require github.com/gin-gonic/gin v1.10.0

require (
	github.com/stretchr/testify v1.9.0 // indirect
	"golang.org/x/net" v0.26.0
	git.corp.example.com/team/lib v0.3.1
	github.com/old/module v1.0.0
	github.com/pinned/module v1.2.0
	github.com/local/module v0.0.0-00010101000000-000000000000
)

replace github.com/old/module => github.com/new/module v1.1.0

replace github.com/pinned/module v1.0.0 => github.com/other/module v9.9.9

replace (
	github.com/local/module => ../local
)

exclude github.com/gin-gonic/gin v1.9.0

retract v0.1.0
