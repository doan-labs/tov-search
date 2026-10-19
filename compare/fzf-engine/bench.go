// fzf behind bench/PROTOCOL.md, on fzf's own matcher: the chunk list,
// pattern, per-chunk cache, partitioned scan and merger the TUI uses, with
// --scheme=path. build.sh drops this file into fzf's src/ at a pinned tag,
// since that code is internal to the package.
//
//   fzf-engine <disk-file>
//
// It indexes files, as fzf's walker does by default, relative to the home
// folder (`cd ~ && fzf`), and prints them back under /Users/bench/.

package fzf

import (
	"bufio"
	"encoding/binary"
	"fmt"
	"os"
	"strings"

	"github.com/junegunn/fzf/src/util"
)

func BenchMain() {
	opts, err := ParseOptions(false, []string{"--scheme=path"})
	if err == nil {
		err = postProcessOptions(opts)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	sortCriteria = opts.Criteria

	b, err := os.ReadFile(os.Args[1])
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	n := int(binary.LittleEndian.Uint64(b[8:16]))
	parent := make([]uint32, n)
	at := make([]int, n)
	ln := make([]int, n)
	cache := NewChunkCache()
	var index int32
	chunks := NewChunkList(cache, func(item *Item, data []byte) bool {
		item.text = util.ToChars(data)
		item.text.Index = index
		index++
		return true
	})
	const home = "/Users/bench/"
	p := 20
	var path []byte
	for i := 0; i < n; i++ {
		parent[i] = binary.LittleEndian.Uint32(b[p:])
		kind := b[p+4]
		ln[i] = int(binary.LittleEndian.Uint16(b[p+17:]))
		at[i] = p + 19
		p += 19 + ln[i]
		if i == 0 || kind&3 == 1 {
			continue
		}
		path = path[:0]
		for j := i; j != 0; j = int(parent[j]) {
			path = append(path, reverse(b[at[j]:at[j]+ln[j]])...)
			path = append(path, '/')
		}
		full := string(reverse(path))
		if strings.HasPrefix(full, home) {
			chunks.Push([]byte(full[len(home):]))
		}
	}
	all, _, _ := chunks.Snapshot(0)
	fmt.Fprintf(os.Stderr, "fzf-engine: %d files indexed\n", CountItems(all))

	forward, withPos := true, false
	for idx := len(opts.Criteria) - 1; idx > 0; idx-- {
		if opts.Criteria[idx] == byPathname {
			withPos, forward = true, false
		}
	}
	patterns := make(map[string]*Pattern)
	rev := revision{}
	build := func(runes []rune) *Pattern {
		return BuildPattern(cache, patterns, opts.Fuzzy, opts.FuzzyAlgo, opts.Extended, opts.Case, opts.Normalize,
			forward, withPos, true, opts.Nth, opts.Delimiter, rev, runes, map[int32]struct{}{})
	}
	m := NewMatcher(cache, build, true, false, util.NewEventBox(), rev)

	out := bufio.NewWriterSize(os.Stdout, 1<<16)
	out.WriteString("ready\n")
	out.Flush()
	in := bufio.NewScanner(os.Stdin)
	in.Buffer(make([]byte, 1<<16), 1<<20)
	prev := ""
	for in.Scan() {
		q := in.Text()
		// a fresh query starts a fresh session: the cache only helps while typing on
		if !strings.HasPrefix(q, prev) || prev == "" {
			cache.Clear()
			patterns = make(map[string]*Pattern)
		}
		prev = q
		pat := build([]rune(q))
		mg, _ := m.scan(MatchRequest{chunks: all, pattern: pat, final: true, sort: pat.sortable, revision: rev})
		for i := 0; i < mg.Length() && i < 50; i++ {
			r := mg.Get(i)
			fmt.Fprintf(out, "%d\t%s%s\n", i, home, r.item.text.ToString())
		}
		out.WriteString("\n")
		out.Flush()
	}
}

func reverse(s []byte) []byte {
	r := make([]byte, len(s))
	for i, c := range s {
		r[len(s)-1-i] = c
	}
	return r
}
