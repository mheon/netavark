PREFIX ?= /usr/local
DATADIR ?= ${PREFIX}/share
MANDIR ?= $(DATADIR)/man
GO ?= go
GOMD2MAN ?= go-md2man

docs: $(patsubst %.md,%,$(wildcard *.[0-9].md))

%.1: %.1.md
	$(GOMD2MAN) -in $^ -out $@

%.5: %.5.md
	$(GOMD2MAN) -in $^ -out $@

.PHONY: .install.md2man
.install.md2man:
	$(GO) install github.com/cpuguy83/go-md2man/v2@latest

.PHONY: install
install:
	install -d ${DESTDIR}/${MANDIR}/man1
	install -m 0644 *.1 ${DESTDIR}/${MANDIR}/man1
	install -d ${DESTDIR}/${MANDIR}/man5
	install -m 0644 *.5 ${DESTDIR}/${MANDIR}/man5

.PHONY: clean
clean:
	$(RM) *.1
