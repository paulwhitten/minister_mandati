from shop.cart import Cart
from shop import pricing


def main():
    cart = Cart()
    cart.add(10.0, 3)
    cart.add(6.25, 2)
    assert cart.total() == pricing.calc_total(cart.items)
    print(f"total: {cart.total():.2f}")


if __name__ == "__main__":
    main()
