// Minimal invoice template: data comes from `sys.inputs.data` (JSON).
#let data = json(bytes(sys.inputs.at("data", default: "{}")))

#set document(title: [Invoice #data.number])
#set page(paper: "a4", margin: 2cm)
#set text(size: 10pt)

#grid(columns: (1fr, auto),
  [*#data.seller.name* \ #data.seller.address],
  align(right)[= Invoice #data.number \ #data.date],
)
#v(1cm)
*Bill to:* #data.client.name \ #data.client.address
#v(1cm)

#let total = data.items.map(i => i.qty * i.price).sum()
#table(
  columns: (1fr, auto, auto, auto),
  align: (left, right, right, right),
  table.header[*Item*][*Qty*][*Unit price*][*Amount*],
  ..data.items.map(i => (i.label, str(i.qty), [#i.price €], [#(i.qty * i.price) €])).flatten(),
  table.footer[][][*Total*][*#total €*],
)
