XGETTEXT ?= xgettext
MSGINIT ?= msginit
MSGMERGE ?= msgmerge
SED ?= sed
ECHO ?= echo

COPYRIGHT_HOLDER = System Alphabet
VERSION ?= 0.0.1
BUGS_ADDRESS = https://github.com/system-alphabet/systema/issues

SOURCES = $(shell find .. -name "*.rs" -not -path "*/target/*")
POTFILE = messages.pot
PACKAGE_NAME = systema
DEFAULT_CHARSET = UTF-8

$(POTFILE): $(SOURCES)
	$(XGETTEXT) --language=Rust \
		--keyword=t_:1 \
		--keyword=n_:1,2 \
		--add-comments \
		--from-code=$(DEFAULT_CHARSET) \
		--output=$@ \
		--strict \
		--indent \
		--add-location \
		--itstool \
		--no-wrap \
		--copyright-holder="$(COPYRIGHT_HOLDER)" \
		--package-name="$(PACKAGE_NAME)" \
		--package-version="$(VERSION)" \
		--msgid-bugs-address="$(BUGS_ADDRESS)" \
		--verbose \
		$(SOURCES)

.PHONY: new-po
new-po:
	@LANG_VAL="$(LANG)"; \
	if [ -z "$$LANG_VAL" ]; then \
		LANG_VAL="$${LANG}"; \
	fi; \
	if [ -z "$$LANG_VAL" ]; then \
		$(ECHO) "Error: LANG not specified and \$$LANG is empty"; \
		exit 1; \
	fi; \
	case "$$LANG_VAL" in \
		*.*) LANGUAGE="$${LANG_VAL%.*}"; CHARSET="$${LANG_VAL#*.}";; \
		*)   LANGUAGE="$$LANG_VAL"; CHARSET="$(DEFAULT_CHARSET)";; \
	esac; \
	$(ECHO) "Creating $$LANGUAGE.po (charset: $$CHARSET)"; \
	$(MAKE) $(POTFILE); \
	$(MSGINIT) --input=$(POTFILE) --no-wrap --locale=$$LANGUAGE --output=$$LANGUAGE.po --no-translator; \
	$(SED) -i 's/charset=CHARSET/charset='$$CHARSET'/' $$LANGUAGE.po
	$(SED) -i 's/Automatically generated/ChenPi11 <wushengwuxi-msctinoulk@outlook.com>/' $$LANGUAGE.po

.PHONY: update-po
update-po: $(POTFILE)
	@for po in *.po; do \
		if [ -f "$$po" ]; then \
			$(ECHO) "Updating $$po"; \
			$(MSGMERGE) --update --indent --add-location --strict --no-wrap --verbose --backup=off "$$po" $(POTFILE); \
		fi; \
	done
	@$(ECHO) "All po files updated."
