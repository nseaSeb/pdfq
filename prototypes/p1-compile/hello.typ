#let data = json(bytes(sys.inputs.at("data", default: "{}")))
= Facture #data.at("number", default: "?")
Client : *#data.at("client", default: "inconnu")* \
Total : #data.at("total", default: 0) €
