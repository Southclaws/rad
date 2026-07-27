package planner

// The shop fixture: a small e-commerce domain exercising every relational
// feature LIR claims — self-referential FKs, nullable columns, composite
// indexes, bool/int64/float64/text columns, and enough rows that grouped,
// nested, and joined shapes have hand-checkable answers.
//
// The dataset, in full (derive expected outputs from THIS table, not from
// running the query):
//
//	customers (email unique; referrer_id is a nullable self-FK)
//	  id   name    email             tier    created_at  referrer_id
//	  c1   Ada     ada@shop.io       gold    100         NULL
//	  c2   Bob     bob@shop.io       silver  200         c1
//	  c3   Cyn     cyn@shop.io       gold    300         c1
//	  c4   Dee     dee@shop.io       bronze  400         c2
//	  c5   Eli     eli@shop.io       bronze  500         NULL
//
//	products (products_cat_price_idx on category, price)
//	  id   name      category  price  stock  discontinued
//	  p1   Keyboard  gear      80.0   12     false
//	  p2   Mouse     gear      40.0   0      false
//	  p3   Monitor   gear      300.0  5      false
//	  p4   Desk      furniture 450.0  2      false
//	  p5   Chair     furniture 250.0  0      true
//	  p6   Lamp      furniture 35.0   40     false
//	  p7   Notebook  paper     5.0    100    false
//
//	orders (orders_cust_status_idx on customer_id, status;
//	        orders_status_placed_idx on status, placed_at;
//	        discount is nullable — c5 has NO orders)
//	  id   customer_id  status     placed_at  discount
//	  o1   c1           delivered  1000       NULL
//	  o2   c1           shipped    2000       10.0
//	  o3   c2           delivered  1500       NULL
//	  o4   c2           cancelled  1600       NULL
//	  o5   c3           pending    3000       25.0
//	  o6   c4           delivered  2500       NULL
//	  o7   c1           pending    3500       NULL
//
//	order_items (items_order_idx on order_id; items_product_idx on product_id)
//	  id    order_id  product_id  quantity  unit_price
//	  i1    o1        p1          1         80.0
//	  i2    o1        p7          10        5.0
//	  i3    o2        p3          1         300.0
//	  i4    o3        p2          2         40.0
//	  i5    o3        p6          1         35.0
//	  i6    o4        p4          1         450.0
//	  i7    o5        p5          2         250.0
//	  i8    o6        p7          20        5.0
//	  i9    o7        p1          1         80.0
//	  i10   o7        p2          1         40.0
//
//	reviews (reviews_product_idx on product_id, rating; body nullable;
//	         p4, p6, p7 have no reviews)
//	  id   product_id  customer_id  rating  body
//	  r1   p1          c1           5       "great"
//	  r2   p1          c2           4       NULL
//	  r3   p2          c1           2       "meh"
//	  r4   p3          c3           5       "sharp"
//	  r5   p5          c4           1       "broke"
//
// Handy derived facts:
//	order totals (sum qty*unit_price): o1=130, o2=300, o3=115, o4=450,
//	  o5=500, o6=100, o7=120
//	orders per customer: c1=3 (o1,o2,o7), c2=2 (o3,o4), c3=1 (o5),
//	  c4=1 (o6), c5=0
//	avg rating per reviewed product: p1=4.5, p2=2, p3=5, p5=1

import (
	"testing"

	"github.com/Southclaws/rad/tests/harness"
)

func shop(t *testing.T) *harness.DB {
	d := harness.New(t)

	d.Table("customers",
		harness.Text("id"), harness.Text("name"), harness.Text("email"),
		harness.Text("tier"), harness.Int64("created_at"),
		harness.Null(harness.Text("referrer_id")),
	).
		Unique("customers_email_uq", "email").
		FK("customers_referrer_fk", "referrer_id", "customers", "id").
		Create()

	d.Table("products",
		harness.Text("id"), harness.Text("name"), harness.Text("category"),
		harness.Float64("price"), harness.Int64("stock"), harness.Bool("discontinued"),
	).
		Index("products_cat_price_idx", "category", "price").
		Create()

	d.Table("orders",
		harness.Text("id"), harness.Text("customer_id"), harness.Text("status"),
		harness.Int64("placed_at"), harness.Null(harness.Float64("discount")),
	).
		Index("orders_cust_status_idx", "customer_id", "status").
		Index("orders_status_placed_idx", "status", "placed_at").
		FK("orders_customer_fk", "customer_id", "customers", "id").
		Create()

	d.Table("order_items",
		harness.Text("id"), harness.Text("order_id"), harness.Text("product_id"),
		harness.Int64("quantity"), harness.Float64("unit_price"),
	).
		Index("items_order_idx", "order_id").
		Index("items_product_idx", "product_id").
		FK("items_order_fk", "order_id", "orders", "id").
		FK("items_product_fk", "product_id", "products", "id").
		Create()

	d.Table("reviews",
		harness.Text("id"), harness.Text("product_id"), harness.Text("customer_id"),
		harness.Int64("rating"), harness.Null(harness.Text("body")),
	).
		Index("reviews_product_idx", "product_id", "rating").
		FK("reviews_product_fk", "product_id", "products", "id").
		FK("reviews_customer_fk", "customer_id", "customers", "id").
		Create()

	d.Insert("customers",
		harness.Row{"id": "c1", "name": "Ada", "email": "ada@shop.io", "tier": "gold", "created_at": 100},
		harness.Row{"id": "c2", "name": "Bob", "email": "bob@shop.io", "tier": "silver", "created_at": 200, "referrer_id": "c1"},
		harness.Row{"id": "c3", "name": "Cyn", "email": "cyn@shop.io", "tier": "gold", "created_at": 300, "referrer_id": "c1"},
		harness.Row{"id": "c4", "name": "Dee", "email": "dee@shop.io", "tier": "bronze", "created_at": 400, "referrer_id": "c2"},
		harness.Row{"id": "c5", "name": "Eli", "email": "eli@shop.io", "tier": "bronze", "created_at": 500},
	)
	d.Insert("products",
		harness.Row{"id": "p1", "name": "Keyboard", "category": "gear", "price": 80.0, "stock": 12, "discontinued": false},
		harness.Row{"id": "p2", "name": "Mouse", "category": "gear", "price": 40.0, "stock": 0, "discontinued": false},
		harness.Row{"id": "p3", "name": "Monitor", "category": "gear", "price": 300.0, "stock": 5, "discontinued": false},
		harness.Row{"id": "p4", "name": "Desk", "category": "furniture", "price": 450.0, "stock": 2, "discontinued": false},
		harness.Row{"id": "p5", "name": "Chair", "category": "furniture", "price": 250.0, "stock": 0, "discontinued": true},
		harness.Row{"id": "p6", "name": "Lamp", "category": "furniture", "price": 35.0, "stock": 40, "discontinued": false},
		harness.Row{"id": "p7", "name": "Notebook", "category": "paper", "price": 5.0, "stock": 100, "discontinued": false},
	)
	d.Insert("orders",
		harness.Row{"id": "o1", "customer_id": "c1", "status": "delivered", "placed_at": 1000},
		harness.Row{"id": "o2", "customer_id": "c1", "status": "shipped", "placed_at": 2000, "discount": 10.0},
		harness.Row{"id": "o3", "customer_id": "c2", "status": "delivered", "placed_at": 1500},
		harness.Row{"id": "o4", "customer_id": "c2", "status": "cancelled", "placed_at": 1600},
		harness.Row{"id": "o5", "customer_id": "c3", "status": "pending", "placed_at": 3000, "discount": 25.0},
		harness.Row{"id": "o6", "customer_id": "c4", "status": "delivered", "placed_at": 2500},
		harness.Row{"id": "o7", "customer_id": "c1", "status": "pending", "placed_at": 3500},
	)
	d.Insert("order_items",
		harness.Row{"id": "i1", "order_id": "o1", "product_id": "p1", "quantity": 1, "unit_price": 80.0},
		harness.Row{"id": "i2", "order_id": "o1", "product_id": "p7", "quantity": 10, "unit_price": 5.0},
		harness.Row{"id": "i3", "order_id": "o2", "product_id": "p3", "quantity": 1, "unit_price": 300.0},
		harness.Row{"id": "i4", "order_id": "o3", "product_id": "p2", "quantity": 2, "unit_price": 40.0},
		harness.Row{"id": "i5", "order_id": "o3", "product_id": "p6", "quantity": 1, "unit_price": 35.0},
		harness.Row{"id": "i6", "order_id": "o4", "product_id": "p4", "quantity": 1, "unit_price": 450.0},
		harness.Row{"id": "i7", "order_id": "o5", "product_id": "p5", "quantity": 2, "unit_price": 250.0},
		harness.Row{"id": "i8", "order_id": "o6", "product_id": "p7", "quantity": 20, "unit_price": 5.0},
		harness.Row{"id": "i9", "order_id": "o7", "product_id": "p1", "quantity": 1, "unit_price": 80.0},
		harness.Row{"id": "i10", "order_id": "o7", "product_id": "p2", "quantity": 1, "unit_price": 40.0},
	)
	d.Insert("reviews",
		harness.Row{"id": "r1", "product_id": "p1", "customer_id": "c1", "rating": 5, "body": "great"},
		harness.Row{"id": "r2", "product_id": "p1", "customer_id": "c2", "rating": 4},
		harness.Row{"id": "r3", "product_id": "p2", "customer_id": "c1", "rating": 2, "body": "meh"},
		harness.Row{"id": "r4", "product_id": "p3", "customer_id": "c3", "rating": 5, "body": "sharp"},
		harness.Row{"id": "r5", "product_id": "p5", "customer_id": "c4", "rating": 1, "body": "broke"},
	)
	return d
}
