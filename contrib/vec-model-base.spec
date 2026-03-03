Name:           vec-model-base
Version:        1.0.0
Release:        1%{?dist}
Summary:        Embedding model for vec (gte-multilingual-base, 50+ languages)
License:        Apache-2.0
URL:            https://github.com/kvzp/vec
# Tarball produced by the vec project's release workflow.
# Contains: model_int8.onnx, tokenizer.json
Source0:        gte-multilingual-base.tar.gz

BuildArch:      noarch

# No build requirements — this package installs pre-built ONNX files only.

%description
gte-multilingual-base ONNX embedding model and tokenizer for vec.

Supports 50+ languages. ~90 MB download. Versioned independently from the
vec binary — a model upgrade requires 'vec updatedb --full' to re-embed.

Install alongside vec:
  dnf install vec vec-model-base

%prep
%setup -q -n gte-multilingual-base

%install
install -D -m 0644 model_int8.onnx \
    %{buildroot}%{_datadir}/vec/models/gte-multilingual-base/model_int8.onnx
install -D -m 0644 tokenizer.json \
    %{buildroot}%{_datadir}/vec/models/gte-multilingual-base/tokenizer.json

%posttrans
# After installing or upgrading the model, trigger a full re-index.
# Old embeddings are incompatible with the new model weights.
# Run in the background so the package manager does not block.
if command -v vec >/dev/null 2>&1 && [ -d /var/lib/vec ]; then
    vec updatedb --full >/dev/null 2>&1 &
fi

%files
%{_datadir}/vec/models/gte-multilingual-base/model_int8.onnx
%{_datadir}/vec/models/gte-multilingual-base/tokenizer.json

%changelog
* Tue Mar 03 2026 Gilles <gdevos@gmail.com> - 1.0.0-1
- Initial packaging
